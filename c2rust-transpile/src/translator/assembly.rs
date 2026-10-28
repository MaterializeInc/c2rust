#![deny(missing_docs)]
//! This module provides basic support for converting inline assembly statements.

use std::collections::HashSet;
use std::fmt::{self as fmt, Display, Formatter};

use crate::diagnostics::TranslationResult;

use super::*;
use log::warn;
use proc_macro2::{TokenStream, TokenTree};
use syn::__private::ToTokens;

/// An argument direction specifier for a Rust asm! expression
enum ArgDirSpec {
    In,
    Out,
    InOut,
    LateOut,
    InLateOut,
}

impl Display for ArgDirSpec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        use ArgDirSpec::*;
        write!(
            f,
            "{}",
            match self {
                In => "in",
                Out => "out",
                InOut => "inout",
                LateOut => "lateout",
                InLateOut => "inlateout",
            }
        )
    }
}

impl ArgDirSpec {
    fn with_in(&self) -> Self {
        use ArgDirSpec::*;
        match self {
            In => In,
            Out => InOut,
            InOut => InOut,
            LateOut => InLateOut,
            InLateOut => InLateOut,
        }
    }
}

/// A machine architecture that rustc inline assembly knows about
#[derive(Copy, Clone, PartialEq)]
enum Arch {
    X86,
    X86_64,
    Arm,
    Aarch64,
    Riscv,
}

/// Parse a machine architecture from a target tuple. This is a best-effort attempt.
fn parse_arch(target_tuple: &str) -> Option<Arch> {
    if target_tuple.starts_with("x86_64") {
        Some(Arch::X86_64)
    } else if target_tuple.starts_with("i386")
        || target_tuple.starts_with("i486")
        || target_tuple.starts_with("i586")
        || target_tuple.starts_with("i686")
        || target_tuple.starts_with("x86")
    {
        Some(Arch::X86)
    } else if target_tuple.starts_with("aarch64")
        || target_tuple.starts_with("armv8")
        || target_tuple.starts_with("arm64")
    {
        Some(Arch::Aarch64)
    } else if target_tuple.starts_with("arm") || target_tuple.starts_with("thumbv") {
        Some(Arch::Arm)
    } else if target_tuple.starts_with("riscv") {
        Some(Arch::Riscv)
    } else {
        None
    }
}

fn parse_constraints(
    mut constraints: &str,
    arch: Arch,
) -> TranslationResult<(ArgDirSpec, bool, String)> {
    let parse_error = |constraints| {
        Err(TranslationError::new(
            None,
            failure::err_msg(
                "Inline assembly constraints could not be parsed: ".to_owned() + constraints,
            )
            .context(TranslationErrorKind::Generic),
        ))
    };
    use ArgDirSpec::*;
    let mut is_input = match constraints.chars().next() {
        Some('+') => {
            constraints = &constraints[1..];
            true
        }
        Some('=') => {
            constraints = &constraints[1..];
            false
        }
        _ => true,
    };

    let early_clobber = if constraints.starts_with('&') {
        constraints = &constraints[1..];
        true
    } else {
        false
    };

    let mut mem_only = if constraints.starts_with('*') {
        constraints = &constraints[1..];
        true
    } else {
        false
    };

    let mut split = constraints.splitn(2, ',');
    constraints = match split.next() {
        Some(c) => c,
        // Parse error
        _ => return parse_error(constraints),
    };
    // If a comma is present, this is an output of form =[&]foo,N
    if split.next().is_some() {
        if !is_input {
            is_input = true;
        } else {
            // '+' followed by ',' is a parse error
            return parse_error(constraints);
        }
    }

    // Handle register names
    let constraints = constraints.replace(['{', '}'], "\"");
    let mut llvm_constraints = constraints.clone();
    let mut constraints = constraints.as_str();

    // Convert (simple) constraints to ones rustc understands
    while !constraints.is_empty() {
        let (c, rest) = constraints.split_at(1);
        let c = c.chars().next().unwrap();
        match c {
            'm' => {
                mem_only = true;
                llvm_constraints = "reg".into();
            }
            'r' => {
                llvm_constraints = "reg".into();
            }
            'i' => {
                // Rust inline assembly has no constraint for that, but uses the argument as an
                // immediate value anyway
                llvm_constraints = "reg".into();
            }
            _ => {
                let is_explicit_reg = c == '"';
                let is_tied = !constraints.contains(|c: char| !c.is_ascii_digit());

                if !(is_explicit_reg || is_tied) {
                    // Attempt to parse machine-specific constraints
                    if let Some((machine_constraints, is_mem)) =
                        translate_machine_constraint(constraints, arch)
                    {
                        llvm_constraints = machine_constraints.into();
                        mem_only = is_mem;
                    } else {
                        warn!(
                            "Did not recognize inline asm constraint: {}\n\
                            It is likely that this will cause compilation errors or \
                            incorrect semantics in the translated program; please \
                            manually correct.",
                            constraints
                        );
                        llvm_constraints = constraints.into();
                    }
                }
                break;
            }
        }
        constraints = rest;
    }

    let mode = if mem_only {
        In
    } else {
        match (is_input, early_clobber) {
            (false, false) => LateOut,
            (false, true) => Out,
            (true, false) => InLateOut,
            (true, true) => InOut,
        }
    };

    Ok((mode, mem_only, llvm_constraints))
}

fn is_regname_or_int(parsed_constraint: &str) -> bool {
    parsed_constraint.contains('"') || parsed_constraint.starts_with(|c: char| c.is_ascii_digit())
}

/// Translate an architecture-specific assembly constraint from llvm/gcc
/// to those accepted by the Rust asm! macro. "Simple" (arch-independent)
/// constraints are handled in `parse_constraints`, not here.
/// See <https://gcc.gnu.org/onlinedocs/gcc/Machine-Constraints.html>,
/// <https://llvm.org/docs/LangRef.html#constraint-codes>, and
/// <https://doc.rust-lang.org/nightly/reference/inline-assembly.html#register-operands>
fn translate_machine_constraint(constraint: &str, arch: Arch) -> Option<(&str, bool)> {
    let mem = &mut false;
    // Many constraints are not handled here, because rustc does. The best we can
    let constraint = match arch {
        Arch::X86 | Arch::X86_64 => match constraint {
            // "R" => "reg_word", // rust does not support this
            "Q" => "reg_abcd",
            "q" => "reg_byte",
            "a" => "\"a\"",
            "b" => "\"b\"",
            "c" => "\"c\"",
            "d" => "\"d\"",
            "S" => "\"si\"",
            "D" => "\"di\"",
            // "A" => "a_and_d", // rust does not support this
            "U" => {
                warn!(
                    "the x86 'U' inline assembly operand constraint cannot \
                be translated correctly. It corresponds to the `clobber_abi` \
                option for `asm!`, but c2rust does not know the ABI being \
                used, so it cannot be translated automatically. Please correct \
                manually after translation."
                );
                return None;
            }
            "f" => "x87_reg",
            "t" => "\"st(0)\"",
            "u" => "\"st(1)\"",
            "x" => "xmm_reg", // this could also translate as ymm_reg
            "y" => "mmx_reg",
            "v" => "zmm_reg",
            "Yz" => "\"xmm0\"",
            "Yk" => "kreg",

            _ => return None,
        },
        Arch::Aarch64 => match constraint {
            "k" => "\"SP\"",
            "w" => "vreg",
            "x" => "vreg_low16",
            // "y" => "vreg_low8", // rust does not support this
            // "Upl" => "preg_low8", // rust does not support this
            "Upa" => "preg",
            "Q" => {
                *mem = true;
                "reg"
            }
            "Ump" => {
                *mem = true;
                "reg"
            }
            _ => return None,
        },
        Arch::Arm => match constraint {
            // "h" => "reg_8_15", // rust does not support this
            "k" => "\"SP\"",
            "l" => "reg",
            "t" => "sreg",
            "x" => "sreg_low16",
            "w" => "dreg",
            // "y" => "sreg",
            // "z" => "sreg",
            "Q" => {
                *mem = true;
                "reg"
            }
            "Uv" | "Uy" | "Uq" => {
                *mem = true;
                "reg"
            }
            _ => return None,
        },
        Arch::Riscv => match constraint {
            "f" => "freg",
            _ => return None,
        },
    };
    Some((constraint, *mem))
}

/// Translate a template modifier from llvm/gcc asm template argument modifiers
/// to those accepted by the Rust asm! macro. This is arch-dependent, so we need
/// to know which architecture the asm targets.
/// See <https://doc.rust-lang.org/nightly/reference/inline-assembly.html#template-modifiers>
fn translate_modifier(modifier: char, arch: Arch) -> Option<char> {
    Some(match arch {
        Arch::X86 | Arch::X86_64 => match modifier {
            'k' => 'e',
            'q' => 'r',
            'b' => 'l',
            'h' => 'h',
            'w' => 'x',
            _ => return None,
        },
        Arch::Aarch64 => modifier,
        Arch::Arm => match modifier {
            'p' | 'q' => return None,
            _ => modifier,
        },
        Arch::Riscv => modifier,
    })
}

/// Rust-native asm! operands, which may be inputs, outputs, or both.
struct BidirAsmOperand {
    dir_spec: ArgDirSpec,
    mem_only: bool,
    constraints: String,
    name: Option<String>,
    // At least one of these is non-None, except for a scratch output, which is discarded (`_`)
    in_expr: Option<(usize, CExprId)>,
    out_expr: Option<(usize, CExprId)>,
}

impl BidirAsmOperand {
    /// Return whether an operand is positional (as opposed to named or using an explicit register)
    fn is_positional(&self) -> bool {
        !self.constraints.contains('"') && self.name.is_none()
    }

    /// Return whether this operand occurs at the given position in the original sequence of
    /// [outputs...inputs]. For tied operands, both input and output index is considered.
    fn has_orig_idx(&self, orig_idx: usize) -> bool {
        match (self.out_expr, self.in_expr) {
            (Some((idx, _)), _) if idx == orig_idx => true,
            (_, Some((idx, _))) if idx == orig_idx => true,
            _ => false,
        }
    }
}

/// A register that rustc refuses as an `asm!` operand or clobber but that C inline assembly
/// may name, e.g. `rbx` on x86_64, which LLVM reserves for its own use.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ReservedReg {
    /// The full-width register, e.g. `rbx` for any of `bl`, `bh`, `bx`, `ebx` and `rbx`.
    full: &'static str,
    /// The template modifier that formats a `reg` operand at the register's full width.
    full_mod: char,
}

/// If `reg` (a register name, without quotes or `%`) is one that rustc refuses in `asm!`,
/// return `Ok` with the register to save and restore around the template,
/// or `Err` with the reason if it can't be saved and restored.
fn reserved_reg(reg: &str, arch: Arch) -> Option<Result<ReservedReg, String>> {
    let saved = |full, full_mod| Some(Ok(ReservedReg { full, full_mod }));
    let unsupported = |what: &str| {
        Some(Err(format!(
            "inline assembly uses {what} ({reg}), which Rust's asm! cannot use \
            as an operand or clobber, and which cannot be saved and restored around the template"
        )))
    };
    match arch {
        Arch::X86_64 => match reg {
            // rustc: "rbx is used internally by LLVM and cannot be used as an operand"
            "bl" | "bh" | "bx" | "ebx" | "rbx" => saved("rbx", 'r'),
            // rustc: "the frame pointer cannot be used as an operand"
            "bpl" | "bp" | "ebp" | "rbp" => saved("rbp", 'r'),
            "spl" | "sp" | "esp" | "rsp" => unsupported("the stack pointer"),
            "ip" | "eip" | "rip" => unsupported("the instruction pointer"),
            _ => None,
        },
        Arch::X86 => match reg {
            // rustc: "esi is used internally by LLVM and cannot be used as an operand"
            "si" | "esi" => saved("esi", 'e'),
            "bp" | "ebp" => saved("ebp", 'e'),
            "sp" | "esp" => unsupported("the stack pointer"),
            "ip" | "eip" => unsupported("the instruction pointer"),
            _ => None,
        },
        _ => None,
    }
}

/// Handle operands and clobbers that use registers rustc refuses in `asm!` (see
/// [`reserved_reg`]), which C allows, e.g. the `%ebx` clobber of a `cpuid`.
///
/// Each such register is saved to a scratch register (a named `out(reg) _` operand) before
/// the template and restored from it afterwards, as `core::arch::x86_64::__cpuid` does for
/// `rbx`, so the code around the asm sees it unchanged, as it would if the compiler had
/// honored the clobber.
///
/// An operand constrained to such a register is instead constrained to any register, and
/// copied into the reserved register after the save (if it's an input) and out of it before
/// the restore (if it's an output). This requires reordering the operands, because we convert
/// them to named operands, which must precede explicit register operands.
///
/// Modifies `operands` and returns the prolog and epilog to wrap the template in, and the
/// indices of the rewritten operands, which the template must not reference, since in C
/// such a reference names the reserved register itself.
fn rewrite_reserved_regs(
    att_syntax: bool,
    arch: Arch,
    operands: &mut Vec<BidirAsmOperand>,
    clobbers: &[String],
) -> TranslationResult<(String, String, Vec<usize>)> {
    let mov = |dst: &str, src: &str| {
        if att_syntax {
            format!("mov {src}, {dst}")
        } else {
            format!("mov {dst}, {src}")
        }
    };
    let reg_ref = |reg: &ReservedReg| {
        if att_syntax {
            format!("%{}", reg.full)
        } else {
            reg.full.to_owned()
        }
    };
    let operand_ref = |name: &str, reg: &ReservedReg| format!("{{{name}:{}}}", reg.full_mod);
    let lookup = |reg: &str| match reserved_reg(reg, arch) {
        Some(Err(e)) => Err(format_translation_err!(None, "{e}")),
        Some(Ok(reserved)) => Ok(Some(reserved)),
        None => Ok(None),
    };

    let mut saved: Vec<ReservedReg> = vec![];
    let (mut copy_in, mut copy_out) = (vec![], vec![]);

    // Determine which operands must be rewritten and how many
    // positional operands there are. Positional operands must precede named
    // operands, so this tells us where to reinsert operands we rewrite.
    let mut rewrite_idxs = vec![];
    let mut total_positional = 0;
    for (i, operand) in operands.iter().enumerate() {
        if operand.is_positional() {
            total_positional += 1;
            continue;
        }
        if !operand.constraints.starts_with('"') {
            continue;
        }
        let reg = operand.constraints.trim_matches('"');
        if let Some(reserved) = lookup(reg)? {
            if reg == "bh" {
                // Copying the full register would move bits 0..8, not 8..16.
                return Err(TranslationError::generic(
                    "inline assembly operand uses bh, which Rust's asm! cannot use \
                    as an operand on x86_64",
                ));
            }
            rewrite_idxs.push((i, reserved));
        }
    }

    let mut rewritten = vec![];
    for (n_moved, (idx, reserved)) in rewrite_idxs.into_iter().enumerate() {
        let operand = &mut operands[idx];
        let name = format!("restmp{}", n_moved);
        // Copy at full width: C leaves the bits beyond the operand's type unspecified,
        // so this is equivalent, and it reads back the narrower value in the low bits.
        // A `+` operand is an input through its dir spec, with only an `out_expr`.
        let is_input = operand.in_expr.is_some()
            || matches!(
                operand.dir_spec,
                ArgDirSpec::In | ArgDirSpec::InOut | ArgDirSpec::InLateOut
            );
        if is_input {
            copy_in.push(mov(&reg_ref(&reserved), &operand_ref(&name, &reserved)));
        }
        if operand.out_expr.is_some() {
            copy_out.push(mov(&operand_ref(&name, &reserved), &reg_ref(&reserved)));
        }
        if !saved.contains(&reserved) {
            saved.push(reserved);
        }
        operand.constraints = "reg".into();
        operand.name = Some(name);

        // Move operand to after all positional arguments. This does not
        // interfere with moving subsequent operands that use reserved registers
        // because explicit register operands must all come after positional
        // and named operands.
        let nth_non_positional = total_positional + n_moved;
        operands.swap(idx, nth_non_positional);
        rewritten.push(nth_non_positional);
    }

    for clobber in clobbers {
        if let Some(reserved) = lookup(clobber)? {
            if !saved.contains(&reserved) {
                saved.push(reserved);
            }
        }
    }

    let (mut prolog, mut epilog) = (vec![], vec![]);
    let insert_at = total_positional + rewritten.len();
    for (n, reserved) in saved.iter().enumerate() {
        let name = format!("c2rust_save_{}", reserved.full);
        prolog.push(mov(&operand_ref(&name, reserved), &reg_ref(reserved)));
        epilog.push(mov(&reg_ref(reserved), &operand_ref(&name, reserved)));
        // A scratch output that isn't late, so it can't share a register with any input or
        // output, holds the saved value across the whole template.
        operands.insert(
            insert_at + n,
            BidirAsmOperand {
                dir_spec: ArgDirSpec::Out,
                mem_only: false,
                constraints: "reg".into(),
                name: Some(name),
                in_expr: None,
                out_expr: None,
            },
        );
    }
    prolog.extend(copy_in);
    copy_out.extend(epilog);
    let epilog = copy_out;

    let prolog = prolog.into_iter().map(|line| line + "\n").collect();
    let epilog = epilog
        .into_iter()
        .map(|line| "\n".to_owned() + &line)
        .collect();
    Ok((prolog, epilog, rewritten))
}

/// Remove comments from an x86 assembly template. Used only to provide a less-
/// confounding input to our Intel-vs-AT&T detection in `asm_is_att_syntax`.
fn remove_comments(mut asm: &str) -> String {
    // Remove C-style comments
    let mut without_c_comments = String::with_capacity(asm.len());
    while let Some(comment_begin) = asm.find("/*") {
        let comment_len = asm[comment_begin..]
            .find("*/")
            // Comments with no terminator extend to the end of the string
            .unwrap_or_else(|| asm[comment_begin..].len());
        let before_comment = &asm[..comment_begin];
        without_c_comments.push_str(before_comment);
        asm = &asm[comment_begin + comment_len..];
    }
    // Push whatever is left after the final comment
    without_c_comments.push_str(asm);

    // Remove EOL comments from each line
    let mut without_comments = String::with_capacity(without_c_comments.len());
    for line in without_c_comments.lines() {
        if let Some(line_comment_idx) = line.find('#') {
            without_comments.push_str(&line[..line_comment_idx]);
        } else {
            without_comments.push_str(line);
        }
        without_comments.push('\n');
    }
    without_comments
}

/// Detect whether an x86(_64) extended asm template string uses AT&T syntax (vs. Intel).
/// For gcc, AT&T syntax is default... unless `-masm=intel` is passed. This
/// means we can hope but not guarantee that x86 asm with no syntax directive
/// uses AT&T syntax.
/// To handle other cases, try to heuristically detect the variant we get
/// (assuming it's actually x86 asm in the first place...).
/// As the rust x86 default is intel syntax, we need to emit the "att_syntax"
/// option if we get a hint that this asm uses AT&T syntax.
///
/// Note that this function receives the asm template after Clang's translation
/// from gcc syntax (with `%` for substitution/escapes) to LLVM syntax which
/// uses `$` for substitution/escapes. See:
/// <https://llvm.org/docs/LangRef.html#inline-assembler-expressions>
fn asm_is_att_syntax(asm: &str) -> bool {
    // First, remove comments, so we can look at only the semantically
    // significant parts of the asm template.
    let asm = &*remove_comments(asm);

    // Look for syntax directives.
    let intel_directive = asm.find(".intel_syntax");
    let att_directive = asm.find(".att_syntax");
    match (intel_directive, att_directive) {
        (Some(intel_pos), Some(att_pos)) => {
            // Both directives are present; presumably this asm switches to one at
            // its start and restores the default at the end. Whichever comes first
            // should be what the asm uses.
            att_pos < intel_pos
        }
        (Some(_intel), None) => false,
        (None, Some(_att)) => true,
        (None, None) => {
            #[allow(clippy::needless_bool)]
            if asm.contains("word ptr") {
                false
            } else if asm.contains("$$") || asm.contains('%') || asm.contains('(') {
                // Guess based on sigils used in AT&T assembly:
                // $ (escaped) for constants, % for registers, and ( for address calculations
                true
            } else if asm.contains('[') {
                // default to true, because AT&T is the default for gcc inline asm
                false
            } else {
                true
            }
        }
    }
}

/// If applicable, maps the index of an input operand to the
/// output operand it is tied to.
///
/// Utilizes the fact that GNU inline assembly specifies all
/// operands in a particular order of [output1, ... outputN]
/// followed by [input1, ..., inputN] where the indices for all
/// input operands are indexed relative to the size of the
/// output operand sequence.
fn tied_output_operand_idx(
    idx: usize,
    num_output_operands: usize,
    tied_operands: &HashMap<(usize, bool), usize>,
) -> usize {
    if let Some(adj_idx) = idx.checked_sub(num_output_operands) {
        // will only be Some(idx) if it was an input operand
        match tied_operands.get(&(adj_idx, false)) {
            Some(&out_idx) => {
                return out_idx;
            }
            None => {
                // get number of tied inputs before this index
                // TODO: can calculate this once before the call, not once for each input
                let num_tied_before = tied_operands
                    .keys()
                    .filter(|&&(iidx, is_out)| !is_out && iidx < adj_idx)
                    .count();
                // shift the original index by the number of tied input operands prior to the current one
                return idx - num_tied_before;
            }
        };
    }

    idx
}

/// Return the operand indices referenced as `{N}` or `{N:modifier}` in an asm! template,
/// skipping escaped braces (`{{`).
fn referenced_operand_indices(template: &str) -> HashSet<usize> {
    let mut indices = HashSet::new();
    let mut chars = template.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c != '{' {
            continue;
        }
        if let Some(&(_, '{')) = chars.peek() {
            chars.next();
            continue;
        }
        let digits: String = template[start + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(idx) = digits.parse() {
            indices.insert(idx);
        }
    }
    indices
}

/// Rewrite a LLVM inline assembly template string into an asm!-compatible one
/// by translating its references to operands (of the form $0 or $x0) to {0} or
/// {0:y} (and wrapping mem-only references in square brackets).
fn rewrite_asm<F: Fn(&str) -> bool, M: Fn(usize) -> usize>(
    asm: &str,
    att_syntax: bool,
    input_op_mapper: M,
    is_mem_only: F,
    arch: Arch,
) -> TranslationResult<String> {
    let mut out = String::with_capacity(asm.len());

    let mut first = true;
    let mut last_empty = false;

    // Iterate over $-prefixed chunks
    for chunk in asm.split('$') {
        // No modification needed for first chunk
        if first {
            first = false;
            out.push_str(chunk);
            continue;
        }

        // Pass-through $$ as one $
        if last_empty {
            last_empty = false;
            out.push('$');
            out.push_str(chunk);
            continue;
        }

        // Note empty chunks
        if chunk.is_empty() {
            last_empty = true;
            continue;
        }

        // Do not re-wrap ${...}, but do translate modifiers
        if chunk.starts_with('{') {
            // Translate operand modifiers ("template modifiers" per Rust)
            if let Some(end_idx) = chunk.find('}') {
                let ref_str = &chunk[..end_idx];
                if let Some(colon_idx) = ref_str.find(':') {
                    let (before_mods, _modifiers) = ref_str.split_at(colon_idx + 1);
                    out.push('{');
                    let idx: usize = before_mods
                        .trim_matches(|c: char| !c.is_ascii_digit())
                        .parse()
                        .map_err(|_| TranslationError::generic("could not parse operand idx"))?;
                    out.push_str(input_op_mapper(idx).to_string().as_str());
                    out.push(':');
                    let modifiers = ref_str[colon_idx + 1..].chars();
                    for modifier in modifiers {
                        if let Some(new) = translate_modifier(modifier, arch) {
                            out.push(new);
                        }
                    }
                    out.push_str(&chunk[end_idx..]);
                }
            } else {
                out.push_str(chunk);
            }
            continue;
        }

        // Translate references of the form %k0 or %3, which look like $k0
        // or $3 in LLVM asm.
        if chunk.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            // Find the end of the reference itself (after 'k0' or '3').
            let end_idx = chunk
                .find(|c: char| c == ',' || !c.is_ascii_alphanumeric())
                .unwrap_or(chunk.len());
            let ref_str = &chunk[..end_idx];

            let index_str;
            let mut new_modifiers = String::new();
            // If the ref string starts with a letter, it's a modifier to translate.
            if let Some(true) = ref_str.chars().next().map(|c| c.is_ascii_alphabetic()) {
                let (modifiers, index) = ref_str.split_at(1);

                index_str = index;

                for modifier in modifiers.chars() {
                    if let Some(new) = translate_modifier(modifier, arch) {
                        new_modifiers.push(new);
                    }
                }
            } else {
                // Just digits
                index_str = ref_str;
            }
            let mem_only = is_mem_only(index_str);
            // Push the reference wrapped in {}, or in [{}] if mem-only
            if mem_only {
                out.push(if att_syntax { '(' } else { '[' });
            };
            out.push('{');
            let idx: usize = index_str
                .parse()
                .map_err(|_| TranslationError::generic("could not parse operand idx"))?;
            out.push_str(input_op_mapper(idx).to_string().as_str());
            if !new_modifiers.is_empty() {
                out.push(':');
                out.push_str(&new_modifiers);
            }
            out.push('}');
            if mem_only {
                out.push(if att_syntax { ')' } else { ']' });
            };
            // Push the rest of the chunk
            out.push_str(&chunk[end_idx..]);
            continue;
        }

        // We failed to parse this operand reference
        out.push_str(chunk);
    }
    Ok(out)
}

impl<'c> Translation<'c> {
    /// Convert an inline-assembly statement into one or more Rust statements.
    /// If inline assembly translation is not enabled this will result in an
    /// error message instead of a conversion. Because the inline assembly syntax
    /// used in C is different than the one used in Rust (Rust uses the LLVM syntax
    /// directly) the resulting translated assembly statements will be unlikely to work
    /// without further manual translation. The translator will properly translate
    /// the arguments to the assembly statement, however.
    pub fn convert_asm(
        &self,
        ctx: ExprContext,
        span: Span,
        is_volatile: bool,
        asm: &str,
        inputs: &[AsmOperand],
        outputs: &[AsmOperand],
        clobbers: &[String],
    ) -> TranslationResult<Vec<Stmt>> {
        if !self.tcfg.translate_asm {
            return Err(TranslationError::generic(
                "Inline assembly translation not enabled.",
            ));
        }

        let arch = match parse_arch(&self.ast_context.target) {
            Some(arch) => arch,
            None => {
                return Err(TranslationError::generic(
                    "Cannot translate inline assembly for unfamiliar architecture",
                ))
            }
        };

        self.use_feature("asm");

        fn push_expr(tokens: &mut Vec<TokenTree>, expr: Box<Expr>) {
            tokens.extend(expr.to_token_stream());
        }

        let mut stmts: Vec<Stmt> = vec![];
        let mut post_stmts: Vec<Stmt> = vec![];
        let mut tokens: Vec<TokenTree> = vec![];

        // Identify tied operands
        let mut tied_operands = HashMap::new();
        for (input_idx, AsmOperand { constraints, .. }) in inputs.iter().enumerate() {
            let constraints_digits = constraints.trim_matches(|c: char| !c.is_ascii_digit());
            if let Ok(output_idx) = constraints_digits.parse::<usize>() {
                let output_key = (output_idx, true);
                // Positional references in template count across outputs then inputs.
                // Add output count so that our index is into the full sequence and can be used for
                // positional template substitutions.
                let input_key = (input_idx + outputs.len(), false);
                tied_operands.insert(output_key, input_idx);
                tied_operands.insert(input_key, output_idx);
            }
        }

        let operand_is_mem_only = |operand: &AsmOperand| -> bool {
            if let Ok((_dir_spec, mem_only, _parsed)) =
                parse_constraints(&operand.constraints, arch)
            {
                mem_only
            } else {
                println!("could not parse asm constraints: {}", operand.constraints);
                false
            }
        };

        // Detect and pair inputs/outputs that constrain themselves to the same register
        let mut inputs_by_register = HashMap::new();
        let mut other_inputs = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            let combined_idx = i + outputs.len();
            let (_dir_spec, _mem_only, parsed) = parse_constraints(&input.constraints, arch)?;
            // Only pair operands with an explicit register or index
            if is_regname_or_int(&parsed) {
                inputs_by_register.insert(parsed, (combined_idx, input.clone()));
            } else {
                other_inputs.push((parsed, (combined_idx, input.clone())));
            }
        }

        // Convert gcc asm arguments (input and output lists) into a single list
        // of operands with explicit arg dir specs (asm!-style)

        // The unified arg list
        let mut args = Vec::new();

        // Add outputs as inout if a matching input is found, else as outputs
        for (output_idx, output) in outputs.iter().enumerate() {
            match parse_constraints(&output.constraints, arch) {
                Ok((mut dir_spec, mem_only, parsed)) => {
                    // Add to args list; if a matching in_expr is found, this is
                    // an inout and we remove the output from the outputs list
                    let mut in_expr = inputs_by_register.remove(&parsed);
                    if in_expr.is_none() {
                        // Also check for by-index references to this output
                        in_expr = inputs_by_register.remove(&output_idx.to_string());
                    }
                    // Extract expression
                    let in_expr = in_expr.map(|(i, operand)| (i, operand.expression));

                    // For inouts, change the dirspec to include 'in'
                    if in_expr.is_some() {
                        dir_spec = dir_spec.with_in();
                    }
                    args.push(BidirAsmOperand {
                        dir_spec,
                        mem_only,
                        name: None,
                        constraints: parsed,
                        in_expr,
                        out_expr: Some((output_idx, output.expression)),
                    });
                }
                // Constraint could not be parsed, drop it
                Err(e) => eprintln!("{}", e),
            }
        }
        // Add unmatched inputs
        for (_, (input_idx, input)) in inputs_by_register
            .into_iter()
            .chain(other_inputs.into_iter())
        {
            let (dir_spec, mem_only, parsed) = match parse_constraints(&input.constraints, arch) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("{}", e);
                    continue;
                }
            };
            args.push(BidirAsmOperand {
                dir_spec,
                mem_only,
                name: None,
                constraints: parsed,
                in_expr: Some((input_idx, input.expression)),
                out_expr: None,
            });
        }

        // Determine whether the assembly is in AT&T syntax
        let att_syntax = match arch {
            Arch::X86 | Arch::X86_64 => asm_is_att_syntax(asm),
            _ => false,
        };

        // Sort positional args before named ones.
        args.sort_by_key(|arg| !arg.is_positional());

        // Save and restore reserved registers (e.g. rbx on x86_64) that operands or clobbers use
        let (prolog, epilog, reserved_operand_idxs) =
            rewrite_reserved_regs(att_syntax, arch, &mut args, clobbers)?;

        // Find new idx by searching args for one with this original idx
        let new_idx_for_orig = |orig_idx| {
            args.iter()
                .position(|operand| operand.has_orig_idx(orig_idx))
                .unwrap_or_else(|| panic!("no operand had index {orig_idx} in asm str:\n{asm}"))
        };

        // Rewrite arg references in assembly template
        let rewritten_asm = rewrite_asm(
            asm,
            att_syntax,
            |idx: usize| {
                new_idx_for_orig(tied_output_operand_idx(idx, outputs.len(), &tied_operands))
            },
            |ref_str: &str| {
                if let Ok(idx) = ref_str.parse::<usize>() {
                    outputs
                        .iter()
                        .chain(inputs.iter())
                        .nth(idx)
                        .map(operand_is_mem_only)
                        .unwrap_or(false)
                } else {
                    false
                }
            },
            arch,
        )?;

        // In C, a reference to an operand constrained to a reserved register names that
        // register, which the template may also use implicitly (as `cpuid` does), and we can't
        // tell which; the rewritten operand only holds the value outside the template.
        let referenced = referenced_operand_indices(&rewritten_asm);
        if reserved_operand_idxs
            .iter()
            .any(|idx| referenced.contains(idx))
        {
            return Err(TranslationError::generic(
                "inline assembly template references an operand constrained to a register \
                that Rust's asm! cannot use as an operand (e.g. rbx on x86_64)",
            ));
        }

        let mut rewritten_asm = prolog + &rewritten_asm + &epilog;

        // Rust rejects positional operands the template never references ("argument never
        // used"); C doesn't, e.g. `asm volatile("" : "+r"(x))` as an optimization barrier.
        // Reference them in an asm comment, as rustc suggests, keeping the operand semantics.
        let referenced = referenced_operand_indices(&rewritten_asm);
        let unused = args
            .iter()
            .enumerate()
            .filter(|(idx, operand)| operand.is_positional() && !referenced.contains(idx))
            .map(|(idx, _)| format!("{{{idx}}}"))
            .collect::<Vec<_>>();
        if !unused.is_empty() {
            rewritten_asm.push_str(&format!("\n/* {} */", unused.join(" ")));
        }

        // Emit assembly template
        for line in rewritten_asm.split('\n') {
            push_expr(&mut tokens, mk().lit_expr(line.to_string() + "\n"));
            tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
        }
        tokens.pop();

        // Outputs and Inputs
        let mut operand_renames = HashMap::new();
        for operand in args {
            tokens.push(TokenTree::Punct(Punct::new(',', Alone)));

            // First, convert output expr if present
            let out_expr = if let Some((output_idx, out_expr)) = operand.out_expr {
                let mut out_expr = self.convert_expr(ctx.used(), out_expr, None)?;
                stmts.append(out_expr.stmts_mut());
                let mut out_expr = out_expr.into_value();

                if operand.mem_only {
                    // If the constraint string contains `*`, then
                    // c2rust-ast-exporter added it (there's no gcc equivalent);
                    // in this case, we need to do what clang does and pass in
                    // the operand by-address instead of by-value
                    self.use_feature("raw_ref_op");
                    out_expr = mk().mutbl().raw_borrow_expr(out_expr);
                }

                if let Some(_tied_operand) = tied_operands.get(&(output_idx, true)) {
                    // If we have an input operand tied to an output operand,
                    // we need to replicate clang's behavior: the inline assembly
                    // uses the larger type internally, and the smaller value gets
                    // extended to the larger one before the call, and truncated
                    // back after (if needed). For portability, we moved the
                    // type conversions into the `c2rust-asm-casts` crate,
                    // so we call into that one from here.

                    // Convert `x` into `let c2rust_output = &raw mut x; *x`
                    self.use_feature("raw_ref_op");
                    let output_name = self
                        .renamer
                        .borrow_mut()
                        .pick_name("c2rust_output", Namespaces::values());
                    let output_local = mk().local(
                        mk().ident_pat(&output_name),
                        None,
                        Some(mk().mutbl().raw_borrow_expr(out_expr)),
                    );
                    stmts.push(mk().local_stmt(Box::new(output_local)));

                    // `let mut c2rust_inner;`
                    let inner_name = self
                        .renamer
                        .borrow_mut()
                        .pick_name("c2rust_inner", Namespaces::values());
                    let inner_local = mk().local(mk().ident_pat(&inner_name), None, None);
                    stmts.push(mk().local_stmt(Box::new(inner_local)));

                    out_expr = mk().ident_expr(&inner_name);
                    operand_renames.insert(output_idx, (output_name, inner_name));
                }
                Some(out_expr)
            } else {
                None
            };

            // Then, handle input expr if present
            let in_expr = if let Some((input_idx, in_expr)) = operand.in_expr {
                let mut in_expr = self.convert_expr(ctx.used(), in_expr, None)?;
                stmts.append(in_expr.stmts_mut());
                let mut in_expr = in_expr.into_value();

                if operand.mem_only {
                    self.use_feature("raw_ref_op");
                    in_expr = mk().raw_borrow_expr(in_expr);
                }
                if let Some(tied_operand) = tied_operands.get(&(input_idx, false)) {
                    self.use_crate(ExternCrate::C2RustAsmCasts);

                    // Import the trait into scope
                    self.with_cur_file_item_store(|item_store| {
                        item_store.add_use(true, vec!["c2rust_asm_casts".into()], "AsmCastTrait");
                    });

                    let (output_name, inner_name) = operand_renames.get(tied_operand).unwrap();

                    let input_name = self
                        .renamer
                        .borrow_mut()
                        .pick_name("c2rust_input", Namespaces::values());
                    let input_local = mk().local(mk().ident_pat(&input_name), None, Some(in_expr));
                    stmts.push(mk().local_stmt(Box::new(input_local)));

                    // Replace `in_expr` with
                    // `c2rust_asm_casts::AsmCast::cast_in(output, input)`
                    let path_expr = mk().path_expr(vec!["c2rust_asm_casts", "AsmCast", "cast_in"]);
                    let output = mk().ident_expr(output_name);
                    let input = mk().ident_expr(input_name);
                    in_expr = mk().call_expr(path_expr, vec![output.clone(), input.clone()]);

                    // Append the cast-out call after the assembly macro:
                    // `c2rust_asm_casts::AsmCast::cast_out(output, input, inner);`
                    let path_expr = mk().path_expr(vec!["c2rust_asm_casts", "AsmCast", "cast_out"]);
                    let inner = mk().ident_expr(inner_name);
                    let cast_out = mk().call_expr(path_expr, vec![output, input, inner]);
                    post_stmts.push(mk().semi_stmt(cast_out));
                }
                Some(in_expr)
            } else {
                None
            };

            // Emit "name =" if a name is given
            if let Some(name) = operand.name {
                push_expr(&mut tokens, mk().ident_expr(name));
                tokens.push(TokenTree::Punct(Punct::new('=', Alone)));
            }

            // Emit dir_spec(constraint), quoting constraint if needed
            push_expr(&mut tokens, mk().ident_expr(operand.dir_spec.to_string()));
            let constraints_ident = if is_regname_or_int(&operand.constraints) {
                mk().lit_expr(operand.constraints.trim_matches('"'))
            } else {
                mk().ident_expr(operand.constraints)
            };

            // Emit input and/or output expressions, separated by "=>" if both
            push_expr(&mut tokens, mk().paren_expr(constraints_ident));
            if let Some(in_expr) = in_expr {
                let in_expr_span = in_expr.span();
                push_expr(&mut tokens, in_expr);
                if out_expr.is_some() {
                    tokens.push(TokenTree::Punct(Punct::new('=', Joint)));
                    tokens.push(TokenTree::Punct(Punct::new('>', Alone)));
                } else {
                    // If inout but no out expr was given, mark clobbered ('_')
                    if let ArgDirSpec::InOut | ArgDirSpec::InLateOut = operand.dir_spec {
                        tokens.push(TokenTree::Punct(Punct::new('=', Joint)));
                        tokens.push(TokenTree::Punct(Punct::new('>', Alone)));

                        tokens.push(TokenTree::Ident(Ident::new("_", in_expr_span)));
                    }
                }
            }
            if let Some(out_expr) = out_expr {
                push_expr(&mut tokens, out_expr);
            } else if operand.in_expr.is_none() {
                // A scratch output, e.g. for saving a reserved register
                tokens.push(TokenTree::Ident(Ident::new("_", Span::call_site())));
            }
        }

        let mut preserves_flags = true;
        let mut read_only = true;

        // Clobbers
        for clobber in clobbers {
            // Process and drop non-register clobbers
            if clobber == "cc" {
                preserves_flags = false;
                continue;
            };
            if clobber == "memory" {
                read_only = false;
                continue;
            };

            // Reserved registers are saved and restored by `rewrite_reserved_regs` instead
            if reserved_reg(clobber, arch).is_some() {
                continue;
            }

            tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
            let result = mk().call_expr(mk().ident_expr("out"), vec![mk().lit_expr(clobber)]);
            push_expr(&mut tokens, result);
            push_expr(&mut tokens, mk().ident_expr("_"));
        }

        // Options
        {
            let mut options = vec![];
            if preserves_flags {
                options.push(mk().ident_expr("preserves_flags"));
            }
            if !is_volatile {
                // Pure cannot be applied if we have no outputs
                if read_only && (outputs.len() + clobbers.len()) > 0 {
                    options.push(mk().ident_expr("pure"));
                    options.push(mk().ident_expr("readonly"));
                }
                // We never emit [pure, nomem] right now, but it would be nice
            }

            if att_syntax {
                options.push(mk().ident_expr("att_syntax"));
            }

            if !options.is_empty() {
                tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
                let result = mk().call_expr(mk().ident_expr("options"), options);
                push_expr(&mut tokens, result);
            }
        }

        self.with_cur_file_item_store(|item_store| {
            item_store.add_use(true, vec!["core".into(), "arch".into()], "asm");
        });

        let mac = mk().mac(
            mk().path(vec!["asm"]),
            tokens.into_iter().collect::<TokenStream>(),
            MacroDelimiter::Paren(Default::default()),
        );
        let mac = mk().mac_expr(mac);
        let mac = mk().span(span).semi_stmt(mac);
        stmts.push(mac);

        // Push the post-macro statements
        stmts.extend(post_stmts);

        Ok(stmts)
    }
}
