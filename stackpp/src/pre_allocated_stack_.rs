use std::cell::Cell;
use std::io::{Error, ErrorKind};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::Stack;

thread_local! {
    /// A stack growth is triggered by accessing a guard page. This will raise a signal with the OS and
    /// inside the signal handler the stack is extended. There is no other way of passing the currently
    /// used stack to the signal handler except saving it in a thread local variable. Signals generated
    /// in response to hardware exceptions, like SIGSEGV, SIGBUS, SIGILL, .. are called thread-directed
    /// signals and are guaranteed to be handled by the same thread that raised them. I assume the same
    /// is true for windows exception handling.
    /// Every time we want to make the stack available to the signal handler we need to first call the
    /// `give_to_signal` method. To get back the stack we need to call `take_from_signal`.
    pub(crate) static CURRENT_STACK: Cell<Option<PreAllocatedStack>> = Cell::new(None);
}

/// Divdes the stack in 2 parts:
/// * A usable area (from bottom[excluding] to top[including]), that can be written to and read from.
/// * A guarded area (from top to guard_top), when accessed will trigger a OS signal.
/// This allows us to reserver a bigger virtual memory space from the OS, but only marking it read/write
/// once we actually need it. We assume here that virtual memory is cheap and that there is no big cost
/// in pre-allocating a big amount of it. This is generally true on all 64 bit Operating Systems.
/// This is also how Windows internally manages stacks of threads. It just keeps removing the guard page
/// until it reaches the maximum allowed stack size (the limit is kept in a the TIB[1] and checked on an
/// exception if memory beyond all guard pages is accessed).
/// [1] https://en.wikipedia.org/wiki/Win32_Thread_Information_Block
pub struct PreAllocatedStack {
    guard_top: *mut u8,
    top: *mut u8,
    bottom: *mut u8,
}

impl Stack for PreAllocatedStack {
    fn new(total_size: usize) -> Result<Self, Error> {
        unsafe {
            // Add 4 extra pages at the top of the stack if we use the whole size, so that there
            // is enough stack for the exception handler on windows to use if we reach the limit.
            let total_size = total_size + 4 * page_size();
            let guard_top = Self::alloc(total_size)?;
            let bottom = guard_top.add(total_size);
            let top = Self::extend_usable(bottom, page_size())?;
            Ok(Self {
                guard_top,
                top,
                bottom,
            })
        }
    }

    fn stack_pointer_inside_guard(&self, sp: *mut u8) -> bool {
        self.guard_top <= sp && sp < self.top
    }

    fn grow(&mut self) -> Result<(), Error> {
        let usable_size = unsafe { self.bottom.sub(self.top as usize) as usize };
        let total_size = unsafe { self.bottom.sub(self.guard_top as usize) as usize };

        if 2 * usable_size > total_size {
            Err(Error::new(
                ErrorKind::Other,
                format!("Stack maximum reached: {}", total_size),
            ))
        } else {
            self.top = unsafe { PreAllocatedStack::extend_usable(self.top, usable_size)? };
            Ok(())
        }
    }

    fn bottom(&self) -> *mut u8 {
        self.bottom
    }

    fn top(&self) -> *mut u8 {
        self.top
    }

    fn guard_top(&self) -> *mut u8 {
        self.guard_top
    }

    fn give_to_signal(self) {
        CURRENT_STACK.with(|stack| stack.set(Some(self)))
    }

    fn take_from_signal() -> Option<Self> {
        CURRENT_STACK.with(|stack| stack.take())
    }

    /// This signal handler will return true if it handeled the signal. This plays nicely with
    /// WASMTIME's `set_signal_handler`. The conditions under which this signal handler will try
    /// to grow the stack are:
    /// * The signal was of type SIGSEGV or SIGBUS (on Windows of type EXCEPTION_ACCESS_VIOLATION)
    /// * the stack pointer points inside the stack's guarded area
    /// The signal will attempt to grow the stack, if there is not enough guarded space to be used
    /// it will return false to signalise WASMTIME to raise a trap.
    #[cfg(target_family = "unix")]
    unsafe extern "C" fn signal_handler(
        signum: libc::c_int,
        siginfo: *mut libc::siginfo_t,
        _context: *mut libc::c_void,
    ) -> bool {
        // On Darwin, guard page accesses are raised as SIGBUS.
        let expected_guard_page_signal = if cfg!(target_os = "macos") {
            libc::SIGBUS
        } else {
            libc::SIGSEGV
        };
        if signum != expected_guard_page_signal {
            return false;
        }

        debug_assert!(!siginfo.is_null(), "siginfo must not be null");

        CURRENT_STACK.with(|stack| {
            let si_addr = (*siginfo).si_addr;
            let mut stack = match stack.take() {
                Some(stack) => stack,
                None => panic!("Stack's signal handler can't find a stack"),
            };
            if stack.stack_pointer_inside_guard(si_addr as *mut u8) {
                let result = stack.grow();
                if result.is_ok() {
                    stack.give_to_signal();
                    return true;
                }
            }
            stack.give_to_signal();
            return false;
        })
    }
    #[cfg(target_family = "windows")]
    unsafe extern "system" fn signal_handler(_exception_info: winapi::um::winnt::PEXCEPTION_POINTERS) -> bool {
        false // No op on windows

        // use winapi::um::minwinbase::EXCEPTION_GUARD_PAGE;

        // let record = &*(*exception_info).ExceptionRecord;
        // if record.ExceptionCode != EXCEPTION_GUARD_PAGE {
        //     return false;
        // }

        // CURRENT_STACK.with(|stack| {
        //     // The second element of ExceptionInformation contains the address of the violation
        //     let si_addr = record.ExceptionInformation[1];
        //     let mut stack = match stack.take() {
        //         Some(stack) => stack,
        //         None => panic!("Stack's signal handler can't find a stack"),
        //     };
        //     if stack.stack_pointer_inside_guard(si_addr as *mut u8) {
        //         let result = stack.grow();
        //         if result.is_ok() {
        //             stack.give_to_signal();
        //             return true;
        //         }
        //     }
        //     stack.give_to_signal();
        //     return false;
        // })
    }

}

#[cfg(target_family = "unix")]
impl PreAllocatedStack { 
    unsafe fn alloc(size: usize) -> Result<*mut u8, Error> {
        use libc::{mmap, MAP_ANON, MAP_FAILED, MAP_NORESERVE, MAP_PRIVATE, PROT_NONE}; 

        let ptr = mmap(
            ptr::null_mut(),
            size,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANON | MAP_NORESERVE,
            -1,
            0,
        );
        if ptr == MAP_FAILED {
            Err(Error::last_os_error())
        } else {
            Ok(ptr as *mut u8)
        }
    }

    /// Mark the bottom part between `top` and `top_guard` writable.
    /// Notice that when a new stack is allocated, bottom and top are at the same address;
    unsafe fn extend_usable(top: *mut u8, size: usize) -> Result<*mut u8, Error> {
        use libc::{mprotect, PROT_READ, PROT_WRITE};

        if mprotect(
            top.sub(size) as *mut libc::c_void,
            size,
            PROT_READ | PROT_WRITE,
        ) == 0
        {
            Ok(top.sub(size))
        } else {
            Err(Error::last_os_error())
        }
    }
}

#[cfg(target_family = "windows")]
impl PreAllocatedStack {
    unsafe fn alloc(size: usize) -> Result<*mut u8, Error> {
        use winapi::um::memoryapi::VirtualAlloc;
        use winapi::um::winnt::{MEM_RESERVE, PAGE_GUARD, PAGE_READWRITE};

        let ptr = VirtualAlloc(ptr::null_mut(), size, MEM_RESERVE, PAGE_GUARD | PAGE_READWRITE);
        if ptr.is_null() {
            Err(Error::last_os_error())
        } else {
            Ok(ptr as *mut u8)
        }
    }

    unsafe fn extend_usable(top: *mut u8, size: usize) -> Result<*mut u8, Error> {
        use winapi::um::memoryapi::VirtualAlloc;
        use winapi::um::winnt::{MEM_COMMIT, PAGE_READWRITE, PAGE_GUARD};

        if !VirtualAlloc(
            top.sub(size) as *mut winapi::ctypes::c_void,
            size,
            MEM_COMMIT,
            PAGE_READWRITE,
        ).is_null()
        {
            // Add one guard page at top of the *usable* stack.
            if !VirtualAlloc(
                top.sub(size + page_size()) as *mut winapi::ctypes::c_void,
                page_size(),
                MEM_COMMIT,
                PAGE_GUARD | PAGE_READWRITE,
            ).is_null()
            {
                Ok(top.sub(size))
            } else {
                Err(Error::last_os_error())
            }
        } else {
            Err(Error::last_os_error())
        }
    }
}

#[cfg(target_family = "unix")]
impl Drop for PreAllocatedStack {
    fn drop(&mut self) {
        let total_size = unsafe { self.bottom.sub(self.guard_top as usize) as usize };
        let result = unsafe { libc::munmap(self.guard_top as *mut libc::c_void, total_size) };
        debug_assert_eq!(result, 0);
    }
}

#[cfg(target_family = "windows")]
impl Drop for PreAllocatedStack {
    fn drop(&mut self) {
        use winapi::um::memoryapi::VirtualFree;
        use winapi::um::winnt::MEM_RELEASE;
        let result = unsafe { VirtualFree(self.guard_top as *mut winapi::ctypes::c_void, 0, MEM_RELEASE) };
        debug_assert_ne!(result, 0);
    }
}

/// Returns page size in bytes
pub fn page_size() -> usize {
    #[cold]
    #[cfg(target_family = "unix")]
    pub fn sys_page_size() -> usize {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }
    
    #[cold]
    #[cfg(target_family = "windows")]
    pub fn sys_page_size() -> usize {
        use winapi::um::sysinfoapi::{SYSTEM_INFO, LPSYSTEM_INFO};
        use winapi::um::sysinfoapi::GetSystemInfo;

        unsafe { 
            let mut info: SYSTEM_INFO = std::mem::zeroed();
            GetSystemInfo(&mut info as LPSYSTEM_INFO);
            info.dwPageSize as usize
         }
    }

    static PAGE_SIZE_CACHE: AtomicUsize = AtomicUsize::new(0);
    match PAGE_SIZE_CACHE.load(Ordering::Relaxed) {
        0 => {
            // Assure that we are using 4KB pages on all platforms.
            let page_size = sys_page_size();
            assert_eq!(page_size, 4096);

            PAGE_SIZE_CACHE.store(page_size, Ordering::Relaxed);
            page_size
        }
        page_size => page_size,
    }
}