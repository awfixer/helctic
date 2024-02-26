use core::{
    mem,
    ptr::{addr_of, addr_of_mut},
    sync::atomic::AtomicBool,
};

use alloc::sync::Arc;

use crate::{
    interrupt::handler::ScratchRegisters,
    paging::{RmmA, RmmArch, TableKind},
    pop_scratch, push_scratch,
    syscall::FloatRegisters,
};

use core::mem::offset_of;
use spin::Once;
use x86::msr;

/// This must be used by the kernel to ensure that context switches are done atomically
/// Compare and exchange this to true when beginning a context switch on any CPU
/// The `Context::switch_to` function will set it back to false, allowing other CPU's to switch
/// This must be done, as no locks can be held on the stack during switch
pub static CONTEXT_SWITCH_LOCK: AtomicBool = AtomicBool::new(false);

const ST_RESERVED: u128 = 0xFFFF_FFFF_FFFF_0000_0000_0000_0000_0000;

#[cfg(cpu_feature_never = "xsave")]
pub const KFX_ALIGN: usize = 16;

#[cfg(not(cpu_feature_never = "xsave"))]
pub const KFX_ALIGN: usize = 64;

#[derive(Clone, Debug)]
#[repr(C)]
pub struct Context {
    /// RFLAGS register
    rflags: usize,
    /// RBX register
    rbx: usize,
    /// R12 register
    r12: usize,
    /// R13 register
    r13: usize,
    /// R14 register
    r14: usize,
    /// R15 register
    r15: usize,
    /// Base pointer
    rbp: usize,
    /// Stack pointer
    pub(crate) rsp: usize,
    /// FSBASE.
    ///
    /// NOTE: Same fsgsbase behavior as with gsbase.
    pub(crate) fsbase: usize,
    /// GSBASE.
    ///
    /// NOTE: Without fsgsbase, this register will strictly be equal to the register value when
    /// running. With fsgsbase, this is neither saved nor restored upon every syscall (there is no
    /// need to!), and thus it must be re-read from the register before copying this struct.
    pub(crate) gsbase: usize,
}

impl Context {
    pub fn new() -> Context {
        Context {
            rflags: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            fsbase: 0,
            gsbase: 0,
        }
    }

    pub fn set_stack(&mut self, address: usize) {
        self.rsp = address;
    }

    pub unsafe fn signal_stack(&mut self, handler: extern "C" fn(usize), sig: u8) {
        self.push_stack(sig as usize);
        self.push_stack(handler as usize);
        self.push_stack(signal_handler_wrapper as usize);
    }

    pub unsafe fn push_stack(&mut self, value: usize) {
        self.rsp -= mem::size_of::<usize>();
        *(self.rsp as *mut usize) = value;
    }

    pub unsafe fn pop_stack(&mut self) -> usize {
        let value = *(self.rsp as *const usize);
        self.rsp += mem::size_of::<usize>();
        value
    }
}
impl super::Context {
    pub fn get_fx_regs(&self) -> FloatRegisters {
        let mut regs = unsafe { self.kfx.as_ptr().cast::<FloatRegisters>().read() };
        regs._reserved = 0;
        let mut new_st = regs.st_space;
        for st in &mut new_st {
            // Only allow access to the 80 lowest bits
            *st &= !ST_RESERVED;
        }
        regs.st_space = new_st;
        regs
    }

    pub fn set_fx_regs(&mut self, mut new: FloatRegisters) {
        {
            let old = unsafe { &*(self.kfx.as_ptr().cast::<FloatRegisters>()) };
            new._reserved = old._reserved;
            let old_st = new.st_space;
            let mut new_st = new.st_space;
            for (new_st, old_st) in new_st.iter_mut().zip(&old_st) {
                *new_st &= !ST_RESERVED;
                *new_st |= old_st & ST_RESERVED;
            }
            new.st_space = new_st;

            // Make sure we don't use `old` from now on
        }

        unsafe {
            self.kfx.as_mut_ptr().cast::<FloatRegisters>().write(new);
        }
    }
}

pub static EMPTY_CR3: Once<rmm::PhysicalAddress> = Once::new();

// SAFETY: EMPTY_CR3 must be initialized.
pub unsafe fn empty_cr3() -> rmm::PhysicalAddress {
    debug_assert!(EMPTY_CR3.poll().is_some());
    *EMPTY_CR3.get_unchecked()
}

/// Switch to the next context by restoring its stack and registers
pub unsafe fn switch_to(prev: &mut super::Context, next: &mut super::Context) {
    if let Some(ref stack) = next.kstack {
        crate::gdt::set_tss_stack(stack.as_ptr() as usize + stack.len());
    }

    core::arch::asm!(
        alternative2!(
            feature1: "xsaveopt",
            then1: ["
                mov eax, 0xffffffff
                mov edx, eax
                xsaveopt [{prev_fx}]
                xrstor [{next_fx}]
            "],
            feature2: "xsave",
            then2: ["
                mov eax, 0xffffffff
                mov edx, eax
                xsave [{prev_fx}]
                xrstor [{next_fx}]
            "],
            default: ["
                fxsave64 [{prev_fx}]
                fxrstor64 [{next_fx}]
            "]
        ),
        prev_fx = in(reg) prev.kfx.as_mut_ptr(),
        next_fx = in(reg) next.kfx.as_ptr(),
        out("eax") _,
        out("edx") _,
    );

    {
        core::arch::asm!(
            alternative!(
                feature: "fsgsbase",
                then: ["
                    mov rax, [{next}+{fsbase_off}]
                    mov rcx, [{next}+{gsbase_off}]

                    rdfsbase rdx
                    wrfsbase rax
                    swapgs
                    rdgsbase rax
                    wrgsbase rcx
                    swapgs

                    mov [{prev}+{fsbase_off}], rdx
                    mov [{prev}+{gsbase_off}], rax
                "],
                // TODO: Most applications will set FSBASE, but won't touch GSBASE. Maybe avoid
                // wrmsr or even the swapgs+rdgsbase+wrgsbase+swapgs sequence if they are already
                // equal?
                default: ["
                    mov ecx, {MSR_FSBASE}
                    mov rdx, [{next}+{fsbase_off}]
                    mov eax, edx
                    shr rdx, 32
                    wrmsr

                    mov ecx, {MSR_KERNEL_GSBASE}
                    mov rdx, [{next}+{gsbase_off}]
                    mov eax, edx
                    shr rdx, 32
                    wrmsr

                    // {prev}
                "]
            ),
            out("rax") _,
            out("rdx") _,
            out("ecx") _, prev = in(reg) addr_of_mut!(prev.arch), next = in(reg) addr_of!(next.arch),
            MSR_FSBASE = const msr::IA32_FS_BASE,
            MSR_KERNEL_GSBASE = const msr::IA32_KERNEL_GSBASE,
            gsbase_off = const offset_of!(Context, gsbase),
            fsbase_off = const offset_of!(Context, fsbase),
        );
    }

    let this_cpu = crate::cpu_id();

    match next.addr_space {
        // Since Arc essentially just wraps a pointer, in this case a regular pointer (as opposed
        // to dyn or slice fat pointers), and NonNull optimization exists, map_or will hopefully be
        // optimized down to checking prev and next pointers, as next cannot be null.
        Some(ref next_space) => {
            if prev
                .addr_space
                .as_ref()
                .map_or(true, |prev_space| !Arc::ptr_eq(prev_space, next_space))
            {
                if let Some(ref prev_space) = prev.addr_space {
                    prev_space.inner.read().used_by.atomic_clear(this_cpu);
                }
                // This lock needs to be held, because if the address space is write-locked by some
                // context that is e.g. unmapping memory, we either need to switch after its
                // changes have been made, or it needs to know this context is potentially using
                // this address space, at that time.
                let next_space = next_space.inner.read();

                next_space.used_by.atomic_set(this_cpu);
                next_space.table.utable.make_current();
            }
        }
        None => {
            // The next context is kernel-only, so switch to the page table without any user
            // mappings.
            if let Some(ref prev_space) = prev.addr_space {
                prev_space.inner.read().used_by.atomic_clear(this_cpu);
            }
            RmmA::set_table(TableKind::User, empty_cr3());
        }
    }

    switch_to_inner(&mut prev.arch, &mut next.arch)
}

// Check disassembly!
#[naked]
unsafe extern "sysv64" fn switch_to_inner(_prev: &mut Context, _next: &mut Context) {
    use Context as Cx;

    core::arch::asm!(
        // As a quick reminder for those who are unfamiliar with the System V ABI (extern "C"):
        //
        // - the current parameters are passed in the registers `rdi`, `rsi`,
        // - we can modify scratch registers, e.g. rax
        // - we cannot change callee-preserved registers arbitrarily, e.g. rbx, which is why we
        //   store them here in the first place.
        concat!("
        // Save old registers, and load new ones
        mov [rdi + {off_rbx}], rbx
        mov rbx, [rsi + {off_rbx}]

        mov [rdi + {off_r12}], r12
        mov r12, [rsi + {off_r12}]

        mov [rdi + {off_r13}], r13
        mov r13, [rsi + {off_r13}]

        mov [rdi + {off_r14}], r14
        mov r14, [rsi + {off_r14}]

        mov [rdi + {off_r15}], r15
        mov r15, [rsi + {off_r15}]

        mov [rdi + {off_rbp}], rbp
        mov rbp, [rsi + {off_rbp}]

        mov [rdi + {off_rsp}], rsp
        mov rsp, [rsi + {off_rsp}]

        // push RFLAGS (can only be modified via stack)
        pushfq
        // pop RFLAGS into `self.rflags`
        pop QWORD PTR [rdi + {off_rflags}]

        // push `next.rflags`
        push QWORD PTR [rsi + {off_rflags}]
        // pop into RFLAGS
        popfq

        // When we return, we cannot even guarantee that the return address on the stack, points to
        // the calling function, `context::switch`. Thus, we have to execute this Rust hook by
        // ourselves, which will unlock the contexts before the later switch.

        // Note that switch_finish_hook will be responsible for executing `ret`.
        jmp {switch_hook}

        "),

        off_rflags = const(offset_of!(Cx, rflags)),

        off_rbx = const(offset_of!(Cx, rbx)),
        off_r12 = const(offset_of!(Cx, r12)),
        off_r13 = const(offset_of!(Cx, r13)),
        off_r14 = const(offset_of!(Cx, r14)),
        off_r15 = const(offset_of!(Cx, r15)),
        off_rbp = const(offset_of!(Cx, rbp)),
        off_rsp = const(offset_of!(Cx, rsp)),

        switch_hook = sym crate::context::switch_finish_hook,
        options(noreturn),
    );
}
#[allow(dead_code)]
#[repr(packed)]
pub struct SignalHandlerStack {
    scratch: ScratchRegisters,
    handler: extern "C" fn(usize),
    sig: usize,
    rip: usize,
}

#[naked]
unsafe extern "C" fn signal_handler_wrapper() {
    #[inline(never)]
    unsafe extern "C" fn inner(stack: &SignalHandlerStack) {
        (stack.handler)(stack.sig);
    }

    // Push scratch registers
    core::arch::asm!(
        concat!(
            "push rax",
            push_scratch!(),
            "
            mov rdi, rsp
            call {inner}
            ",
            pop_scratch!(),
            "
            add rsp, 16
            ret
            "
        ),
        inner = sym inner,
        options(noreturn)
    );
}
