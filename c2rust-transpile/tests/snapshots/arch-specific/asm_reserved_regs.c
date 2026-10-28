#ifdef __x86_64__

// Rust's asm! can't name rbx (LLVM reserves it), so a clobber or an operand that uses it
// must save and restore rbx around the template rather than drop the clobber.

// cpuid with %ebx only in the clobber list.
unsigned int cpuid_ecx_clobber(void) {
    unsigned int eax = 1, ecx;
    __asm__("cpuid" : "=c"(ecx) : "a"(eax) : "%ebx", "%edx");
    return ecx;
}

// cpuid with ebx as an output operand.
unsigned int cpuid_ebx_operand(void) {
    unsigned int eax = 0, ebx, ecx, edx;
    __asm__("cpuid" : "+a"(eax), "=b"(ebx), "=c"(ecx), "=d"(edx));
    return ebx;
}

// ebx as an input operand.
unsigned int ebx_input(unsigned int x) {
    unsigned int out;
    __asm__("movl %%ebx, %%eax\n\taddl %%ebx, %%eax" : "=a"(out) : "b"(x));
    return out;
}

// ebx as an input and output operand.
unsigned int ebx_inout(unsigned int x) {
    __asm__("addl %%ebx, %%ebx" : "+b"(x));
    return x;
}

#endif
