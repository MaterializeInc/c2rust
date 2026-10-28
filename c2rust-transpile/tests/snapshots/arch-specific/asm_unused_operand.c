// Operands that the template never references are fine in C but rejected by Rust's asm!
// ("argument never used"); the translation must still compile.

// An empty asm used as an optimization barrier on a value.
const void *barrier(const void *x) {
    __asm__ __volatile__("" : "+r"(x));
    return x;
}

// Several unused operands, inputs and outputs.
int barrier2(int a, int b) {
    __asm__ __volatile__("" : "+r"(a) : "r"(b));
    return a;
}
