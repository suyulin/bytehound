use std::mem;
use std::ptr;

use libc::{
    c_void,
    c_int,
    size_t,
    off_t
};

use common::event;
use common::Timestamp;

use crate::ON_APPLICATION_THREAD_DEFAULT;
use crate::InternalEvent;
use crate::on_exit;
use crate::opt;
use crate::syscall;
use crate::timestamp::get_timestamp;
use crate::tls::get_tls;
use crate::unwind::{self, Backtrace};

use crate::{
    acquire_lock,
    send_event,
    send_event_throttled
};

extern "C" {
    #[link_name = "__libc_malloc"]
    fn malloc_real( size: size_t ) -> *mut c_void;
    #[link_name = "__libc_calloc"]
    fn calloc_real( count: size_t, element_size: size_t ) -> *mut c_void;
    #[link_name = "__libc_realloc"]
    fn realloc_real( ptr: *mut c_void, size: size_t ) -> *mut c_void;
    #[link_name = "__libc_free"]
    fn free_real( ptr: *mut c_void );
    #[link_name = "__libc_memalign"]
    fn memalign_real( alignment: size_t, size: size_t ) -> *mut c_void;
    #[link_name = "__libc_mallopt"]
    fn mallopt_real( params: c_int, value: c_int ) -> c_int;

    #[link_name = "__libc_fork"]
    fn fork_real() -> libc::pid_t;
}

fn get_timestamp_if_enabled() -> Timestamp {
    if opt::get().precise_timestamps {
        get_timestamp()
    } else {
        Timestamp::min()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sys_mmap( addr: *mut c_void, length: size_t, prot: c_int, flags: c_int, fildes: c_int, off: off_t ) -> *mut c_void {
    syscall::mmap( addr, length, prot, flags, fildes, off )
}

#[no_mangle]
pub unsafe extern "C" fn sys_munmap( addr: *mut c_void, length: size_t ) -> c_int {
    syscall::munmap( addr, length )
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn _exit( status: c_int ) {
    on_exit();
    syscall::exit( status as u32 );
}

// `libc` on mips64 doesn't export this
extern "C" {
    fn malloc_usable_size( ptr: *mut libc::c_void) -> libc::size_t;
}

fn get_glibc_metadata( ptr: *mut c_void, size: usize ) -> (u32, u32, u64) {
    let raw_chunk_size = unsafe { *(ptr as *mut usize).offset( -1 ) };
    let flags = raw_chunk_size & 0b111;
    let chunk_size = raw_chunk_size & !0b111;

    let is_prev_in_use = flags & 1 != 0;
    let preceding_free_space = if !is_prev_in_use {
        unsafe { *(ptr as *mut usize).offset( -2 ) }
    } else {
        0
    };

    let is_mmapped = flags & 2 != 0;
    let extra_usable_space = chunk_size - size - mem::size_of::< usize >() * if is_mmapped { 2 } else { 1 };

    debug_assert_eq!(
        size + extra_usable_space,
        unsafe { malloc_usable_size( ptr ) },
        "chunk_size: {}, size: {}, malloc_usable_size: {}, extra_usable_space: {}",
        chunk_size,
        size,
        unsafe { malloc_usable_size( ptr ) },
        extra_usable_space,
    );

    (flags as u32, extra_usable_space as u32, preceding_free_space as u64)
}

#[inline(always)]
unsafe fn allocate( size: usize, is_calloc: bool ) -> *mut c_void {
    let lock = acquire_lock();
    let ptr =
        if is_calloc || opt::get().zero_memory {
            calloc_real( size as size_t, 1 )
        } else {
            malloc_real( size as size_t )
        };

    if ptr.is_null() {
        return ptr;
    }

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return ptr };
    let mut backtrace = Backtrace::new();
    unwind::grab( &mut tls, &mut backtrace );

    let (mut flags, extra_usable_space, preceding_free_space) = get_glibc_metadata( ptr, size );
    if is_calloc {
        flags |= event::ALLOC_FLAG_CALLOC;
    }

    let thread = tls.thread_id;
    send_event_throttled( move || {
        InternalEvent::Alloc {
            ptr: ptr as usize,
            size: size as usize,
            backtrace,
            thread,
            flags,
            extra_usable_space,
            preceding_free_space,
            timestamp: get_timestamp_if_enabled(),
            throttle
        }
    });

    mem::drop( tls );
    ptr
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn malloc( size: size_t ) -> *mut c_void {
    allocate( size, false )
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn calloc( count: size_t, element_size: size_t ) -> *mut c_void {
    let size = match (count as usize).checked_mul( element_size as usize ) {
        None => return ptr::null_mut(),
        Some( size ) => size as size_t
    };

    allocate( size, true )
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn realloc( old_ptr: *mut c_void, size: size_t ) -> *mut c_void {
    if old_ptr.is_null() {
        return malloc( size );
    }

    if size == 0 {
        free( old_ptr );
        return ptr::null_mut();
    }

    let lock = acquire_lock();
    let new_ptr = realloc_real( old_ptr, size );

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return new_ptr };
    let mut backtrace = Backtrace::new();
    unwind::grab( &mut tls, &mut backtrace );

    let thread = tls.thread_id;
    let timestamp = get_timestamp_if_enabled();

    if !new_ptr.is_null() {
        let (flags, extra_usable_space, preceding_free_space) = get_glibc_metadata( new_ptr, size );
        send_event_throttled( move || {
            InternalEvent::Realloc {
                old_ptr: old_ptr as usize,
                new_ptr: new_ptr as usize,
                size: size as usize,
                backtrace,
                thread,
                flags,
                extra_usable_space,
                preceding_free_space,
                timestamp,
                throttle
            }
        });
    } else {
        send_event_throttled( || {
            InternalEvent::Free {
                ptr: old_ptr as usize,
                backtrace,
                thread,
                timestamp,
                throttle
            }
        });
    }

    mem::drop( tls );
    new_ptr
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn free( ptr: *mut c_void ) {
    if ptr.is_null() {
        return;
    }

    let lock = acquire_lock();
    free_real( ptr );

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return };
    let mut backtrace = Backtrace::new();
    if opt::get().grab_backtraces_on_free {
        unwind::grab( &mut tls, &mut backtrace );
    }

    let thread = tls.thread_id;
    send_event_throttled( || {
        InternalEvent::Free {
            ptr: ptr as usize,
            backtrace,
            thread,
            timestamp: get_timestamp_if_enabled(),
            throttle
        }
    });

    mem::drop( tls );
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn posix_memalign( memptr: *mut *mut c_void, alignment: size_t, size: size_t ) -> c_int {
    if memptr.is_null() {
        return libc::EINVAL;
    }

    let ptr_size = mem::size_of::< *const c_void >();
    if alignment % ptr_size != 0 || !(alignment / ptr_size).is_power_of_two() || alignment == 0 {
        return libc::EINVAL;
    }

    let lock = acquire_lock();

    let pointer = memalign_real( alignment, size );
    *memptr = pointer;
    if pointer.is_null() {
        return libc::ENOMEM;
    }

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return 0 };
    let mut backtrace = Backtrace::new();
    unwind::grab( &mut tls, &mut backtrace );

    let (flags, extra_usable_space, preceding_free_space) = get_glibc_metadata( pointer, size );
    let thread = tls.thread_id;
    send_event_throttled( || {
        InternalEvent::Alloc {
            ptr: pointer as usize,
            size: size as usize,
            backtrace,
            thread,
            flags,
            extra_usable_space,
            preceding_free_space,
            timestamp: get_timestamp_if_enabled(),
            throttle
        }
    });

    mem::drop( tls );
    0
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn mmap( addr: *mut c_void, length: size_t, prot: c_int, flags: c_int, fildes: c_int, off: off_t ) -> *mut c_void {
    let lock = acquire_lock();

    let ptr = sys_mmap( addr, length, prot, flags, fildes, off );
    if ptr == libc::MAP_FAILED {
        return ptr;
    }

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return ptr };
    let mut backtrace = Backtrace::new();
    unwind::grab( &mut tls, &mut backtrace );

    let thread = tls.thread_id;
    send_event_throttled( || InternalEvent::Mmap {
        pointer: ptr as usize,
        length: length as usize,
        requested_address: addr as usize,
        mmap_protection: prot as u32,
        mmap_flags: flags as u32,
        file_descriptor: fildes as u32,
        offset: off as u64,
        backtrace,
        thread,
        timestamp: get_timestamp_if_enabled(),
        throttle
    });

    mem::drop( tls );
    ptr
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn munmap( ptr: *mut c_void, length: size_t ) -> c_int {
    let lock = acquire_lock();
    let result = sys_munmap( ptr, length );

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return result };
    let mut backtrace = Backtrace::new();
    unwind::grab( &mut tls, &mut backtrace );

    if !ptr.is_null() {
        let thread = tls.thread_id;
        send_event_throttled( || InternalEvent::Munmap {
            ptr: ptr as usize,
            len: length as usize,
            backtrace,
            thread,
            timestamp: get_timestamp_if_enabled(),
            throttle
        });
    }

    mem::drop( tls );
    result
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn mallopt( param: c_int, value: c_int ) -> c_int {
    let lock = acquire_lock();
    let result = mallopt_real( param, value );

    let (mut tls, throttle) = if let Some( lock ) = lock { lock } else { return result };
    let mut backtrace = Backtrace::new();
    unwind::grab( &mut tls, &mut backtrace );

    let thread = tls.thread_id;
    send_event_throttled( || InternalEvent::Mallopt {
        param: param as i32,
        value: value as i32,
        result: result as i32,
        backtrace,
        thread,
        timestamp: get_timestamp_if_enabled(),
        throttle
    });

    mem::drop( tls );
    result
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn fork() -> libc::pid_t {
    let pid = fork_real();
    if pid == 0 {
        let mut tls = get_tls();
        let tls = tls.as_mut().unwrap();
        tls.on_application_thread = false;
        *ON_APPLICATION_THREAD_DEFAULT.lock() = false;
    } else {
        info!( "Fork called; child PID: {}", pid );
    }

    pid
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn memalign( _alignment: size_t, _size: size_t ) -> *mut c_void {
    unimplemented!( "'memalign' is unimplemented!" );
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn aligned_alloc( _alignment: size_t, _size: size_t ) -> *mut c_void {
    unimplemented!( "'aligned_alloc' is unimplemented!" );
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn valloc( _size: size_t ) -> *mut c_void {
    unimplemented!( "'valloc' is unimplemented!" );
}

#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn pvalloc( _size: size_t ) -> *mut c_void {
    unimplemented!( "'pvalloc' is unimplemented!" );
}

#[no_mangle]
pub unsafe extern "C" fn memory_profiler_set_marker( value: u32 ) {
    let lock = acquire_lock();
    send_event( InternalEvent::SetMarker {
        value
    });

    mem::drop( lock );
}

#[no_mangle]
pub unsafe extern "C" fn memory_profiler_override_next_timestamp( timestamp: u64 ) {
    let lock = acquire_lock();
    send_event_throttled( || InternalEvent::OverrideNextTimestamp {
        timestamp: Timestamp::from_usecs( timestamp )
    });
    mem::drop( lock );
}

#[no_mangle]
pub unsafe extern "C" fn memory_profiler_stop() {
    let lock = acquire_lock();
    send_event( InternalEvent::Stop );
    mem::drop( lock );
}