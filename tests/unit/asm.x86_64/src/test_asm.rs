//! extern_crate_c2rust_asm_casts

use crate::asm::{
    rust_cpuid_ebx_operand, rust_cpuid_ecx_clobber, rust_ebx_inout, rust_ebx_input, rust_entry,
};
use std::arch::asm;
use std::arch::x86_64::__cpuid;
use std::ffi::{c_int, c_uint};

#[link(name = "test")]
unsafe extern "C" {
    fn entry(_: c_uint, _: *mut c_int);
    fn cpuid_ecx_clobber() -> c_uint;
    fn cpuid_ebx_operand() -> c_uint;
    fn ebx_input(_: c_uint) -> c_uint;
    fn ebx_inout(_: c_uint) -> c_uint;
}

const BUFFER_SIZE: usize = 6;

#[test]
pub fn test_buffer() {
    let mut buffer = [0; BUFFER_SIZE];
    let mut rust_buffer = [0; BUFFER_SIZE];
    let expected_buffer = [243025, 65070, 51450, 12, 12, 6];

    unsafe {
        entry(BUFFER_SIZE as u32, buffer.as_mut_ptr());
        rust_entry(BUFFER_SIZE as u32, rust_buffer.as_mut_ptr());
    }

    assert_eq!(buffer, rust_buffer);
    assert_eq!(buffer, expected_buffer);
}

/// Call `f` with a canary in rbx, which is callee-saved,
/// and return `f`'s result after checking that rbx still holds the canary.
macro_rules! call_with_rbx_canary {
    ($f:path $(, $arg:expr)?) => {{
        const CANARY: u64 = 0x0123_4567_89ab_cdef;
        let rbx_after: u64;
        let ret: u64;
        unsafe {
            asm!(
                "push rbx",
                // Keep the stack 16-byte aligned for the call.
                "sub rsp, 8",
                "mov rbx, r12",
                "call {f}",
                "mov r12, rbx",
                "add rsp, 8",
                "pop rbx",
                f = sym $f,
                // r12 is callee-saved, so it survives the call.
                inlateout("r12") CANARY => rbx_after,
                $(in("edi") $arg,)?
                lateout("rax") ret,
                clobber_abi("C"),
            );
        }
        assert_eq!(rbx_after, CANARY, "{} did not preserve rbx", stringify!($f));
        ret as c_uint
    }};
}

#[test]
pub fn test_cpuid_ecx_clobber() {
    let expected = __cpuid(1).ecx;
    assert_eq!(unsafe { cpuid_ecx_clobber() }, expected);
    assert_eq!(call_with_rbx_canary!(rust_cpuid_ecx_clobber), expected);
}

#[test]
pub fn test_cpuid_ebx_operand() {
    let expected = __cpuid(0).ebx;
    assert_eq!(unsafe { cpuid_ebx_operand() }, expected);
    assert_eq!(call_with_rbx_canary!(rust_cpuid_ebx_operand), expected);
}

#[test]
pub fn test_ebx_input() {
    assert_eq!(unsafe { ebx_input(21) }, 42);
    assert_eq!(call_with_rbx_canary!(rust_ebx_input, 21u32), 42);
}

#[test]
pub fn test_ebx_inout() {
    assert_eq!(unsafe { ebx_inout(21) }, 42);
    assert_eq!(call_with_rbx_canary!(rust_ebx_inout, 21u32), 42);
}
