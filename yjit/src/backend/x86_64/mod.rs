#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

use crate::asm::{uimm_num_bits, CodeBlock};
use crate::asm::x86_64::*;
use crate::codegen::{JITState};
use crate::cruby::*;
use crate::backend::ir::{Assembler, Opnd, Target, Op, MemBase, Mem};

// Use the x86 register type for this platform
pub type Reg = X86Reg;

// Callee-saved registers
pub const _CFP: Opnd = Opnd::Reg(R13_REG);
pub const _EC: Opnd = Opnd::Reg(R12_REG);
pub const _SP: Opnd = Opnd::Reg(RBX_REG);

// C argument registers on this platform
pub const _C_ARG_OPNDS: [Opnd; 6] = [
    Opnd::Reg(RDI_REG),
    Opnd::Reg(RSI_REG),
    Opnd::Reg(RDX_REG),
    Opnd::Reg(RCX_REG),
    Opnd::Reg(R8_REG),
    Opnd::Reg(R9_REG)
];

// C return value register on this platform
pub const C_RET_REG: Reg = RAX_REG;
pub const _C_RET_OPND: Opnd = Opnd::Reg(RAX_REG);

/// Map Opnd to X86Opnd
impl From<Opnd> for X86Opnd {
    fn from(opnd: Opnd) -> Self {
        match opnd {
            // NOTE: these operand types need to be lowered first
            //Value(VALUE),       // Immediate Ruby value, may be GC'd, movable
            //InsnOut(usize),     // Output of a preceding instruction in this block

            Opnd::InsnOut{..} => panic!("InsnOut operand made it past register allocation"),

            Opnd::None => X86Opnd::None,

            Opnd::UImm(val) => uimm_opnd(val),
            Opnd::Imm(val) => imm_opnd(val),
            Opnd::Value(VALUE(uimm)) => uimm_opnd(uimm as u64),

            // General-purpose register
            Opnd::Reg(reg) => X86Opnd::Reg(reg),

            // Memory operand with displacement
            Opnd::Mem(Mem{ base: MemBase::Reg(reg_no), num_bits, disp }) => {
                let reg = X86Reg {
                    reg_no,
                    num_bits: 64,
                    reg_type: RegType::GP
                };

                mem_opnd(num_bits, X86Opnd::Reg(reg), disp)
            }

            _ => panic!("unsupported x86 operand type")
        }
    }
}

impl Assembler
{
    /// Get the list of registers from which we can allocate on this platform
    pub fn get_alloc_regs() -> Vec<Reg>
    {
        vec![
            RAX_REG,
            RCX_REG,
        ]
    }

    /// Get a list of all of the caller-save registers
    pub fn get_caller_save_regs() -> Vec<Reg> {
        vec![RAX_REG, RCX_REG, RDX_REG, RSI_REG, RDI_REG, R8_REG, R9_REG, R10_REG, R11_REG]

        // Technically these are also caller-save: R12_REG, R13_REG, R14_REG,
        // and R15_REG, but we don't use them so we don't include them here.
    }

    /// Split IR instructions for the x86 platform
    fn x86_split(mut self) -> Assembler
    {
        let live_ranges: Vec<usize> = std::mem::take(&mut self.live_ranges);

        self.forward_pass(|asm, index, op, opnds, target| {
            // Load heap object operands into registers because most
            // instructions can't directly work with 64-bit constants
            let opnds = match op {
                Op::Load | Op::Mov => opnds,
                _ => opnds.into_iter().map(|opnd| {
                    if let Opnd::Value(value) = opnd {
                        if !value.special_const_p() {
                            asm.load(opnd)
                        } else {
                            opnd
                        }
                    } else {
                        opnd
                    }
                }).collect()
            };

            match op {
                Op::Add | Op::Sub | Op::And => {
                    let (opnd0, opnd1) = match (opnds[0], opnds[1]) {
                        (Opnd::Mem(_), Opnd::Mem(_)) => {
                            (asm.load(opnds[0]), asm.load(opnds[1]))
                        },
                        (Opnd::Mem(_), Opnd::UImm(value)) => {
                            if uimm_num_bits(value) > 32 {
                                (asm.load(opnds[0]), asm.load(opnds[1]))
                            } else {
                                (asm.load(opnds[0]), opnds[1])
                            }
                        },
                        // Instruction output whose live range spans beyond this instruction
                        (Opnd::InsnOut { idx, .. }, _) => {
                            if live_ranges[idx] > index {
                                (asm.load(opnds[0]), opnds[1])
                            } else {
                                (opnds[0], opnds[1])
                            }
                        },
                        // We have to load memory and register operands to avoid corrupting them
                        (Opnd::Mem(_) | Opnd::Reg(_), _) => {
                            (asm.load(opnds[0]), opnds[1])
                        },
                        _ => (opnds[0], opnds[1])
                    };

                    asm.push_insn(op, vec![opnd0, opnd1], target);
                },
                Op::Mov => {
                    match (opnds[0], opnds[1]) {
                        (Opnd::Mem(_), Opnd::Mem(_)) => {
                            // We load opnd1 because for mov, opnd0 is the output
                            let opnd1 = asm.load(opnds[1]);
                            asm.mov(opnds[0], opnd1);
                        },
                        (Opnd::Mem(_), Opnd::UImm(value)) => {
                            if uimm_num_bits(value) > 32 {
                                let opnd1 = asm.load(opnds[1]);
                                asm.mov(opnds[0], opnd1);
                            } else {
                                asm.mov(opnds[0], opnds[1]);
                            }
                        },
                        _ => {
                            asm.mov(opnds[0], opnds[1]);
                        }
                    }
                },
                Op::Not => {
                    let opnd0 = match opnds[0] {
                        // If we have an instruction output whose live range
                        // spans beyond this instruction, we have to load it.
                        Opnd::InsnOut { idx, .. } => {
                            if live_ranges[idx] > index {
                                asm.load(opnds[0])
                            } else {
                                opnds[0]
                            }
                        },
                        // We have to load memory and register operands to avoid
                        // corrupting them.
                        Opnd::Mem(_) | Opnd::Reg(_) => asm.load(opnds[0]),
                        // Otherwise we can just reuse the existing operand.
                        _ => opnds[0]
                    };

                    asm.not(opnd0);
                },
                _ => {
                    asm.push_insn(op, opnds, target);
                }
            };
        })
    }

    /// Emit platform-specific machine code
    pub fn x86_emit(&mut self, cb: &mut CodeBlock) -> Vec<u32>
    {
        //dbg!(&self.insns);

        // List of GC offsets
        let mut gc_offsets: Vec<u32> = Vec::new();

        // For each instruction
        for insn in &self.insns {
            match insn.op {
                Op::Comment => {
                    if cfg!(feature = "asm_comments") {
                        cb.add_comment(&insn.text.as_ref().unwrap());
                    }
                },

                // Write the label at the current position
                Op::Label => {
                    cb.write_label(insn.target.unwrap().unwrap_label_idx());
                },

                Op::Add => {
                    add(cb, insn.opnds[0].into(), insn.opnds[1].into())
                },

                Op::Sub => {
                    sub(cb, insn.opnds[0].into(), insn.opnds[1].into())
                },

                Op::And => {
                    and(cb, insn.opnds[0].into(), insn.opnds[1].into())
                },

                Op::Not => {
                    not(cb, insn.opnds[0].into())
                },

                Op::Store => mov(cb, insn.opnds[0].into(), insn.opnds[1].into()),

                // This assumes only load instructions can contain references to GC'd Value operands
                Op::Load => {
                    mov(cb, insn.out.into(), insn.opnds[0].into());

                    // If the value being loaded is a heap object
                    if let Opnd::Value(val) = insn.opnds[0] {
                        if !val.special_const_p() {
                            // The pointer immediate is encoded as the last part of the mov written out
                            let ptr_offset: u32 = (cb.get_write_pos() as u32) - (SIZEOF_VALUE as u32);
                            gc_offsets.push(ptr_offset);
                        }
                    }
                },

                Op::Mov => mov(cb, insn.opnds[0].into(), insn.opnds[1].into()),

                // Load effective address
                Op::Lea => lea(cb, insn.out.into(), insn.opnds[0].into()),

                // Push and pop to/from the C stack
                Op::CPush => push(cb, insn.opnds[0].into()),
                Op::CPop => pop(cb, insn.out.into()),
                Op::CPopInto => pop(cb, insn.opnds[0].into()),

                // Push and pop to the C stack all caller-save registers and the
                // flags
                Op::CPushAll => {
                    let regs = Assembler::get_caller_save_regs();

                    for reg in regs {
                        push(cb, X86Opnd::Reg(reg));
                    }
                    pushfq(cb);
                },
                Op::CPopAll => {
                    let regs = Assembler::get_caller_save_regs();

                    popfq(cb);
                    for reg in regs.into_iter().rev() {
                        pop(cb, X86Opnd::Reg(reg));
                    }
                },

                // C function call
                Op::CCall => {
                    // Temporary
                    assert!(insn.opnds.len() < C_ARG_REGS.len());

                    // For each operand
                    for (idx, opnd) in insn.opnds.iter().enumerate() {
                        mov(cb, C_ARG_REGS[idx], insn.opnds[idx].into());
                    }

                    let ptr = insn.target.unwrap().unwrap_fun_ptr();
                    call_ptr(cb, RAX, ptr);
                },

                Op::CRet => {
                    // TODO: bias allocation towards return register
                    if insn.opnds[0] != Opnd::Reg(C_RET_REG) {
                        mov(cb, RAX, insn.opnds[0].into());
                    }

                    ret(cb);
                }

                // Compare
                Op::Cmp => test(cb, insn.opnds[0].into(), insn.opnds[1].into()),

                // Test and set flags
                Op::Test => test(cb, insn.opnds[0].into(), insn.opnds[1].into()),

                Op::JmpOpnd => jmp_rm(cb, insn.opnds[0].into()),

                // Conditional jump to a label
                Op::Jmp => {
                    match insn.target.unwrap() {
                        Target::CodePtr(code_ptr) => jmp_ptr(cb, code_ptr),
                        Target::Label(label_idx) => jmp_label(cb, label_idx),
                        _ => unreachable!()
                    }
                }

                Op::Je => {
                    match insn.target.unwrap() {
                        Target::CodePtr(code_ptr) => je_ptr(cb, code_ptr),
                        Target::Label(label_idx) => je_label(cb, label_idx),
                        _ => unreachable!()
                    }
                }

                Op::Jz => {
                    match insn.target.unwrap() {
                        Target::CodePtr(code_ptr) => jz_ptr(cb, code_ptr),
                        Target::Label(label_idx) => jz_label(cb, label_idx),
                        _ => unreachable!()
                    }
                }

                Op::Jnz => {
                    match insn.target.unwrap() {
                        Target::CodePtr(code_ptr) => jnz_ptr(cb, code_ptr),
                        Target::Label(label_idx) => jnz_label(cb, label_idx),
                        _ => unreachable!()
                    }
                }

                Op::Jo => {
                    match insn.target.unwrap() {
                        Target::CodePtr(code_ptr) => jo_ptr(cb, code_ptr),
                        Target::Label(label_idx) => jo_label(cb, label_idx),
                        _ => unreachable!()
                    }
                }

                // Atomically increment a counter at a given memory location
                Op::IncrCounter => {
                    assert!(matches!(insn.opnds[0], Opnd::Mem(_)));
                    assert!(matches!(insn.opnds[1], Opnd::UImm(_) | Opnd::Imm(_) ) );
                    write_lock_prefix(cb);
                    add(cb, insn.opnds[0].into(), insn.opnds[1].into());
                },

                Op::Breakpoint => int3(cb),

                _ => panic!("unsupported instruction passed to x86 backend: {:?}", insn.op)
            };
        }

        gc_offsets
    }

    /// Optimize and compile the stored instructions
    pub fn compile_with_regs(self, cb: &mut CodeBlock, regs: Vec<Reg>) -> Vec<u32>
    {
        let mut asm = self.x86_split().alloc_regs(regs);

        // Create label instances in the code block
        for (idx, name) in asm.label_names.iter().enumerate() {
            let label_idx = cb.new_label(name.to_string());
            assert!(label_idx == idx);
        }

        let gc_offsets = asm.x86_emit(cb);

        cb.link_labels();

        gc_offsets
    }
}