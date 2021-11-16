use bumpalo::collections::vec::Vec;
use bumpalo::Bump;
use core::panic;

use roc_module::symbol::Symbol;

use super::linking::{IndexRelocType, OffsetRelocType, RelocationEntry};
use super::opcodes::{OpCode, OpCode::*};
use super::serialize::{SerialBuffer, Serialize};
use crate::{round_up_to_alignment, FRAME_ALIGNMENT_BYTES, STACK_POINTER_GLOBAL_ID};

const ENABLE_DEBUG_LOG: bool = true;
macro_rules! log_instruction {
    ($($x: expr),+) => {
        if ENABLE_DEBUG_LOG { println!($($x,)*); }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalId(pub u32);

/// Wasm value type. (Rust representation matches Wasm encoding)
#[repr(u8)]
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum ValueType {
    I32 = 0x7f,
    I64 = 0x7e,
    F32 = 0x7d,
    F64 = 0x7c,
}

impl Serialize for ValueType {
    fn serialize<T: SerialBuffer>(&self, buffer: &mut T) {
        buffer.append_u8(*self as u8);
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum BlockType {
    NoResult,
    Value(ValueType),
}

impl BlockType {
    pub fn as_byte(&self) -> u8 {
        match self {
            Self::NoResult => 0x40,
            Self::Value(t) => *t as u8,
        }
    }
}

/// A control block in our model of the VM
/// Child blocks cannot "see" values from their parent block
struct VmBlock<'a> {
    /// opcode indicating what kind of block this is
    opcode: OpCode,
    /// the stack of values for this block
    value_stack: Vec<'a, Symbol>,
    /// whether this block pushes a result value to its parent
    has_result: bool,
}

impl std::fmt::Debug for VmBlock<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{:?} {}",
            self.opcode,
            if self.has_result {
                "Result"
            } else {
                "NoResult"
            }
        ))
    }
}

/// Wasm memory alignment. (Rust representation matches Wasm encoding)
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum Align {
    Bytes1 = 0,
    Bytes2 = 1,
    Bytes4 = 2,
    Bytes8 = 3,
    Bytes16 = 4,
    Bytes32 = 5,
    Bytes64 = 6,
    // ... we can add more if we need them ...
}

impl From<u32> for Align {
    fn from(x: u32) -> Align {
        match x {
            1 => Align::Bytes1,
            2 => Align::Bytes2,
            4 => Align::Bytes4,
            8 => Align::Bytes8,
            16 => Align::Bytes16,
            32 => Align::Bytes32,
            64 => Align::Bytes64,
            _ => panic!("{:?}-byte alignment not supported", x),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum VmSymbolState {
    /// Value doesn't exist yet
    NotYetPushed,

    /// Value has been pushed onto the VM stack but not yet popped
    /// Remember where it was pushed, in case we need to insert another instruction there later
    Pushed { pushed_at: usize },

    /// Value has been pushed and popped, so it's not on the VM stack any more.
    /// If we want to use it again later, we will have to create a local for it,
    /// by going back to insert a local.tee instruction at pushed_at
    Popped { pushed_at: usize },
}

// An instruction (local.set or local.tee) to be inserted into the function code
#[derive(Debug)]
struct Insertion {
    at: usize,
    start: usize,
    end: usize,
}

macro_rules! instruction_no_args {
    ($method_name: ident, $opcode: expr, $pops: expr, $push: expr) => {
        pub fn $method_name(&mut self) {
            self.inst($opcode, $pops, $push);
        }
    };
}

macro_rules! instruction_memargs {
    ($method_name: ident, $opcode: expr, $pops: expr, $push: expr) => {
        pub fn $method_name(&mut self, align: Align, offset: u32) {
            self.inst_mem($opcode, $pops, $push, align, offset);
        }
    };
}

#[derive(Debug)]
pub struct CodeBuilder<'a> {
    arena: &'a Bump,

    /// The main container for the instructions
    code: Vec<'a, u8>,

    /// Instruction bytes to be inserted into the code when finalizing the function
    /// (Used for setting locals when we realise they are used multiple times)
    insert_bytes: Vec<'a, u8>,

    /// Code locations where the insert_bytes should go
    insertions: Vec<'a, Insertion>,

    /// Bytes for local variable declarations and stack-frame setup code.
    /// We can't write this until we've finished the main code. But it goes
    /// before it in the final output, so we need a separate vector.
    preamble: Vec<'a, u8>,

    /// Encoded bytes for the inner length of the function, locals + code.
    /// ("inner" because it doesn't include its own length!)
    /// Again, we can't write this until we've finished the code and preamble,
    /// but it goes before them in the binary, so it's a separate vector.
    inner_length: Vec<'a, u8>,

    /// Our simulation model of the Wasm stack machine
    /// Nested blocks of instructions. A child block can't "see" the stack of its parent block
    vm_block_stack: Vec<'a, VmBlock<'a>>,

    /// Linker info to help combine the Roc module with builtin & platform modules,
    /// e.g. to modify call instructions when function indices change
    relocations: Vec<'a, RelocationEntry>,
}

#[allow(clippy::new_without_default)]
impl<'a> CodeBuilder<'a> {
    pub fn new(arena: &'a Bump) -> Self {
        let mut vm_block_stack = Vec::with_capacity_in(8, arena);
        let function_block = VmBlock {
            opcode: BLOCK,
            has_result: true,
            value_stack: Vec::with_capacity_in(8, arena),
        };
        vm_block_stack.push(function_block);

        CodeBuilder {
            arena,
            code: Vec::with_capacity_in(1024, arena),
            insertions: Vec::with_capacity_in(32, arena),
            insert_bytes: Vec::with_capacity_in(64, arena),
            preamble: Vec::with_capacity_in(32, arena),
            inner_length: Vec::with_capacity_in(5, arena),
            vm_block_stack,
            relocations: Vec::with_capacity_in(32, arena),
        }
    }

    /**********************************************************

        SYMBOLS

        The Wasm VM stores temporary values in its stack machine.
        We track which stack positions correspond to IR Symbols,
        because it helps to generate more efficient code.

    ***********************************************************/

    fn current_stack(&self) -> &Vec<'a, Symbol> {
        let block = self.vm_block_stack.last().unwrap();
        &block.value_stack
    }

    fn current_stack_mut(&mut self) -> &mut Vec<'a, Symbol> {
        let block = self.vm_block_stack.last_mut().unwrap();
        &mut block.value_stack
    }

    /// Set the Symbol that is at the top of the VM stack right now
    /// We will use this later when we need to load the Symbol
    pub fn set_top_symbol(&mut self, sym: Symbol) -> VmSymbolState {
        let current_stack = &mut self.vm_block_stack.last_mut().unwrap().value_stack;
        let pushed_at = self.code.len();
        let top_symbol: &mut Symbol = current_stack.last_mut().unwrap();
        *top_symbol = sym;

        VmSymbolState::Pushed { pushed_at }
    }

    /// Verify if a sequence of symbols is at the top of the stack
    pub fn verify_stack_match(&self, symbols: &[Symbol]) -> bool {
        let current_stack = self.current_stack();
        let n_symbols = symbols.len();
        let stack_depth = current_stack.len();
        if n_symbols > stack_depth {
            return false;
        }
        let offset = stack_depth - n_symbols;

        for (i, sym) in symbols.iter().enumerate() {
            if current_stack[offset + i] != *sym {
                return false;
            }
        }
        true
    }

    fn add_insertion(&mut self, insert_at: usize, opcode: OpCode, immediate: u32) {
        let start = self.insert_bytes.len();

        self.insert_bytes.push(opcode as u8);
        self.insert_bytes.encode_u32(immediate);

        self.insertions.push(Insertion {
            at: insert_at,
            start,
            end: self.insert_bytes.len(),
        });

        log_instruction!(
            "**insert {:?} {} at byte offset {}**",
            opcode,
            immediate,
            insert_at
        );
    }

    /// Load a Symbol that is stored in the VM stack
    /// If it's already at the top of the stack, no code will be generated.
    /// Otherwise, local.set and local.get instructions will be inserted, using the LocalId provided.
    ///
    /// If the return value is `Some(s)`, `s` should be stored by the caller, and provided in the next call.
    /// If the return value is `None`, the Symbol is no longer stored in the VM stack, but in a local.
    /// (In this case, the caller must remember to declare the local in the function header.)
    pub fn load_symbol(
        &mut self,
        symbol: Symbol,
        vm_state: VmSymbolState,
        next_local_id: LocalId,
    ) -> Option<VmSymbolState> {
        use VmSymbolState::*;

        match vm_state {
            NotYetPushed => unreachable!("Symbol {:?} has no value yet. Nothing to load.", symbol),

            Pushed { pushed_at } => {
                match self.current_stack().last() {
                    Some(top_symbol) if *top_symbol == symbol => {
                        // We're lucky, the symbol is already on top of the current block's stack.
                        // No code to generate! (This reduces code size by up to 25% in tests.)
                        // Just let the caller know what happened
                        Some(Popped { pushed_at })
                    }
                    _ => {
                        // Symbol is not on top of the stack.
                        // We should have saved it to a local, so go back and do that now.

                        // It should still be on the stack in the block where it was assigned. Remove it.
                        let mut found = false;
                        for block in self.vm_block_stack.iter_mut() {
                            if let Some(found_index) =
                                block.value_stack.iter().position(|&s| s == symbol)
                            {
                                block.value_stack.remove(found_index);
                                found = true;
                            }
                        }

                        // Go back to the code position where it was pushed, and save it to a local
                        if found {
                            self.add_insertion(pushed_at, SETLOCAL, next_local_id.0);
                        } else {
                            if ENABLE_DEBUG_LOG {
                                println!(
                                    "{:?} has been popped implicitly. Leaving it on the stack.",
                                    symbol
                                );
                            }
                            self.add_insertion(pushed_at, TEELOCAL, next_local_id.0);
                        }

                        // Recover the value again at the current position
                        self.get_local(next_local_id);
                        self.set_top_symbol(symbol);

                        // This Symbol is no longer stored in the VM stack, but in a local
                        None
                    }
                }
            }

            Popped { pushed_at } => {
                // This Symbol is being used for a second time
                // Insert a local.tee where it was pushed, so we don't interfere with the first usage
                self.add_insertion(pushed_at, TEELOCAL, next_local_id.0);

                // Insert a local.get at the current position
                self.get_local(next_local_id);
                self.set_top_symbol(symbol);

                // This symbol has been promoted to a Local
                // Tell the caller it no longer has a VirtualMachineSymbolState
                None
            }
        }
    }

    /**********************************************************

        FUNCTION HEADER

    ***********************************************************/

    /// Generate bytes to declare the function's local variables
    fn build_local_declarations(&mut self, local_types: &[ValueType]) {
        // reserve one byte for num_batches
        self.preamble.push(0);

        if local_types.is_empty() {
            return;
        }

        // Write declarations in batches of the same ValueType
        let mut num_batches: u32 = 0;
        let mut batch_type = local_types[0];
        let mut batch_size = 0;
        for t in local_types {
            if *t == batch_type {
                batch_size += 1;
            } else {
                self.preamble.encode_u32(batch_size);
                self.preamble.push(batch_type as u8);
                batch_type = *t;
                batch_size = 1;
                num_batches += 1;
            }
        }
        self.preamble.encode_u32(batch_size);
        self.preamble.push(batch_type as u8);
        num_batches += 1;

        // Go back and write the number of batches at the start
        if num_batches < 128 {
            self.preamble[0] = num_batches as u8;
        } else {
            // We need more than 1 byte to encode num_batches!
            // This is a ridiculous edge case, so just pad to 5 bytes for simplicity
            let old_len = self.preamble.len();
            self.preamble.resize(old_len + 4, 0);
            self.preamble.copy_within(1..old_len, 5);
            self.preamble.overwrite_padded_u32(0, num_batches);
        }
    }

    /// Generate instruction bytes to grab a frame of stack memory on entering the function
    fn build_stack_frame_push(&mut self, frame_size: i32, frame_pointer: LocalId) {
        // Can't use the usual instruction methods because they push to self.code.
        // This is the only case where we push instructions somewhere different.
        self.preamble.push(GETGLOBAL as u8);
        self.preamble.encode_u32(STACK_POINTER_GLOBAL_ID);
        self.preamble.push(I32CONST as u8);
        self.preamble.encode_i32(frame_size);
        self.preamble.push(I32SUB as u8);
        self.preamble.push(TEELOCAL as u8);
        self.preamble.encode_u32(frame_pointer.0);
        self.preamble.push(SETGLOBAL as u8);
        self.preamble.encode_u32(STACK_POINTER_GLOBAL_ID);
    }

    /// Generate instruction bytes to release a frame of stack memory on leaving the function
    fn build_stack_frame_pop(&mut self, frame_size: i32, frame_pointer: LocalId) {
        self.get_local(frame_pointer);
        self.i32_const(frame_size);
        self.i32_add();
        self.set_global(STACK_POINTER_GLOBAL_ID);
    }

    /// Build the function header: local declarations, stack frame push/pop code, and function length
    /// After this, all bytes have been generated (but not yet serialized) and we know the final size.
    pub fn build_fn_header(
        &mut self,
        local_types: &[ValueType],
        frame_size: i32,
        frame_pointer: Option<LocalId>,
    ) {
        self.build_local_declarations(local_types);

        if let Some(frame_ptr_id) = frame_pointer {
            let aligned_size = round_up_to_alignment(frame_size, FRAME_ALIGNMENT_BYTES);
            self.build_stack_frame_push(aligned_size, frame_ptr_id);
            self.build_stack_frame_pop(aligned_size, frame_ptr_id);
        }

        self.code.push(END as u8);

        let inner_len = self.preamble.len() + self.code.len() + self.insert_bytes.len();
        self.inner_length.encode_u32(inner_len as u32);

        // Sort insertions. They are not created in order of assignment, but in order of *second* usage.
        self.insertions.sort_by_key(|ins| ins.at);
    }

    /**********************************************************

        SERIALIZE

    ***********************************************************/

    /// Serialize all byte vectors in the right order
    /// Also update relocation offsets relative to the base offset (code section body start)
    pub fn serialize_with_relocs<T: SerialBuffer>(
        &self,
        buffer: &mut T,
        final_relocs: &mut Vec<'a, RelocationEntry>,
        reloc_base_offset: usize,
    ) {
        buffer.append_slice(&self.inner_length);
        buffer.append_slice(&self.preamble);

        // Do the insertions & update relocation offsets
        let mut reloc_index = 0;
        let mut code_pos = 0;
        let mut insert_iter = self.insertions.iter();

        loop {
            let next_insert = insert_iter.next();
            let next_pos = match next_insert {
                Some(Insertion { at, .. }) => *at,
                None => self.code.len(),
            };

            // Relocation offset needs to be an index into the body of the code section, but
            // at this point it is an index into self.code. Need to adjust for all previous functions
            // in the code section, and for insertions in the current function.
            let section_body_pos = buffer.size() - reloc_base_offset;
            while reloc_index < self.relocations.len()
                && self.relocations[reloc_index].offset() < next_pos as u32
            {
                let mut reloc_clone = self.relocations[reloc_index].clone();
                *reloc_clone.offset_mut() += (section_body_pos - code_pos) as u32;
                final_relocs.push(reloc_clone);
                reloc_index += 1;
            }

            buffer.append_slice(&self.code[code_pos..next_pos]);

            match next_insert {
                Some(Insertion { at, start, end }) => {
                    buffer.append_slice(&self.insert_bytes[*start..*end]);
                    code_pos = *at;
                }
                None => {
                    break;
                }
            }
        }
    }

    /**********************************************************

        INSTRUCTION HELPER METHODS

    ***********************************************************/

    /// Base method for generating instructions
    /// Emits the opcode and simulates VM stack push/pop
    fn inst_base(&mut self, opcode: OpCode, pops: usize, push: bool) {
        let current_stack = self.current_stack_mut();
        let new_len = current_stack.len() - pops as usize;
        current_stack.truncate(new_len);
        if push {
            current_stack.push(Symbol::WASM_TMP);
        }
        self.code.push(opcode as u8);
    }

    /// Plain instruction without any immediates
    fn inst(&mut self, opcode: OpCode, pops: usize, push: bool) {
        self.inst_base(opcode, pops, push);
        log_instruction!(
            "{:10}\t\t{:?}",
            format!("{:?}", opcode),
            self.current_stack()
        );
    }

    /// Block instruction
    fn inst_block(&mut self, opcode: OpCode, pops: usize, block_type: BlockType) {
        self.inst_base(opcode, pops, false);
        self.code.push(block_type.as_byte());

        // Start a new block with a fresh value stack
        self.vm_block_stack.push(VmBlock {
            opcode,
            value_stack: Vec::with_capacity_in(8, self.arena),
            has_result: block_type != BlockType::NoResult,
        });

        log_instruction!(
            "{:10} {:?}\t{:?}",
            format!("{:?}", opcode),
            block_type,
            &self.vm_block_stack
        );
    }

    fn inst_imm32(&mut self, opcode: OpCode, pops: usize, push: bool, immediate: u32) {
        self.inst_base(opcode, pops, push);
        self.code.encode_u32(immediate);
        log_instruction!(
            "{:10}\t{}\t{:?}",
            format!("{:?}", opcode),
            immediate,
            self.current_stack()
        );
    }

    fn inst_mem(&mut self, opcode: OpCode, pops: usize, push: bool, align: Align, offset: u32) {
        self.inst_base(opcode, pops, push);
        self.code.push(align as u8);
        self.code.encode_u32(offset);
        log_instruction!(
            "{:10} {:?} {}\t{:?}",
            format!("{:?}", opcode),
            align,
            offset,
            self.current_stack()
        );
    }

    /// Insert a linker relocation for a memory address
    pub fn insert_memory_relocation(&mut self, symbol_index: u32) {
        self.relocations.push(RelocationEntry::Offset {
            type_id: OffsetRelocType::MemoryAddrLeb,
            offset: self.code.len() as u32,
            symbol_index,
            addend: 0,
        });
    }

    /**********************************************************

        INSTRUCTION METHODS

        One method for each Wasm instruction (in same order as the spec)
        macros are for compactness & readability for the most common cases
        Patterns that don't repeat very much don't have macros

    ***********************************************************/

    instruction_no_args!(unreachable_, UNREACHABLE, 0, false);
    instruction_no_args!(nop, NOP, 0, false);

    pub fn block(&mut self, ty: BlockType) {
        self.inst_block(BLOCK, 0, ty);
    }
    pub fn loop_(&mut self, ty: BlockType) {
        self.inst_block(LOOP, 0, ty);
    }
    pub fn if_(&mut self, ty: BlockType) {
        self.inst_block(IF, 1, ty);
    }
    pub fn else_(&mut self) {
        // Reuse the 'then' block but clear its value stack
        self.current_stack_mut().clear();
        self.inst(ELSE, 0, false);
    }

    pub fn end(&mut self) {
        self.inst_base(END, 0, false);

        let ended_block = self.vm_block_stack.pop().unwrap();
        if ended_block.has_result {
            let result = ended_block.value_stack.last().unwrap();
            self.current_stack_mut().push(*result)
        }

        log_instruction!("END       \t\t{:?}", &self.vm_block_stack);
    }
    pub fn br(&mut self, levels: u32) {
        self.inst_imm32(BR, 0, false, levels);
    }
    pub fn br_if(&mut self, levels: u32) {
        // In dynamic execution, br_if can pop 2 values if condition is true and the target block has a result.
        // But our stack model is for *static* analysis and we need it to be correct at the next instruction,
        // where the branch was not taken. So we only pop 1 value, the condition.
        self.inst_imm32(BRIF, 1, false, levels);
    }
    #[allow(dead_code)]
    fn br_table() {
        panic!("TODO");
    }

    instruction_no_args!(return_, RETURN, 0, false);

    pub fn call(
        &mut self,
        function_index: u32,
        symbol_index: u32,
        n_args: usize,
        has_return_val: bool,
    ) {
        self.inst_base(CALL, n_args, has_return_val);

        let offset = self.code.len() as u32;
        self.code.encode_padded_u32(function_index);

        // Make a RelocationEntry so the linker can see that this byte offset relates to a function by name.
        // Here we initialise the offset to an index of self.code. After completing the function, we'll add
        // other factors to make it relative to the code section. (All insertions will be known then.)
        self.relocations.push(RelocationEntry::Index {
            type_id: IndexRelocType::FunctionIndexLeb,
            offset,
            symbol_index,
        });

        log_instruction!(
            "{:10}\t{}\t{:?}",
            format!("{:?}", CALL),
            function_index,
            self.current_stack()
        );
    }

    #[allow(dead_code)]
    fn call_indirect() {
        panic!("Not implemented. Roc doesn't use function pointers");
    }

    instruction_no_args!(drop_, DROP, 1, false);
    instruction_no_args!(select, SELECT, 3, true);

    pub fn get_local(&mut self, id: LocalId) {
        self.inst_imm32(GETLOCAL, 0, true, id.0);
    }
    pub fn set_local(&mut self, id: LocalId) {
        self.inst_imm32(SETLOCAL, 1, false, id.0);
    }
    pub fn tee_local(&mut self, id: LocalId) {
        self.inst_imm32(TEELOCAL, 0, false, id.0);
    }
    pub fn get_global(&mut self, id: u32) {
        self.inst_imm32(GETGLOBAL, 0, true, id);
    }
    pub fn set_global(&mut self, id: u32) {
        self.inst_imm32(SETGLOBAL, 1, false, id);
    }

    instruction_memargs!(i32_load, I32LOAD, 1, true);
    instruction_memargs!(i64_load, I64LOAD, 1, true);
    instruction_memargs!(f32_load, F32LOAD, 1, true);
    instruction_memargs!(f64_load, F64LOAD, 1, true);
    instruction_memargs!(i32_load8_s, I32LOAD8S, 1, true);
    instruction_memargs!(i32_load8_u, I32LOAD8U, 1, true);
    instruction_memargs!(i32_load16_s, I32LOAD16S, 1, true);
    instruction_memargs!(i32_load16_u, I32LOAD16U, 1, true);
    instruction_memargs!(i64_load8_s, I64LOAD8S, 1, true);
    instruction_memargs!(i64_load8_u, I64LOAD8U, 1, true);
    instruction_memargs!(i64_load16_s, I64LOAD16S, 1, true);
    instruction_memargs!(i64_load16_u, I64LOAD16U, 1, true);
    instruction_memargs!(i64_load32_s, I64LOAD32S, 1, true);
    instruction_memargs!(i64_load32_u, I64LOAD32U, 1, true);
    instruction_memargs!(i32_store, I32STORE, 2, false);
    instruction_memargs!(i64_store, I64STORE, 2, false);
    instruction_memargs!(f32_store, F32STORE, 2, false);
    instruction_memargs!(f64_store, F64STORE, 2, false);
    instruction_memargs!(i32_store8, I32STORE8, 2, false);
    instruction_memargs!(i32_store16, I32STORE16, 2, false);
    instruction_memargs!(i64_store8, I64STORE8, 2, false);
    instruction_memargs!(i64_store16, I64STORE16, 2, false);
    instruction_memargs!(i64_store32, I64STORE32, 2, false);

    pub fn memory_size(&mut self) {
        self.inst(CURRENTMEMORY, 0, true);
        self.code.push(0);
    }
    pub fn memory_grow(&mut self) {
        self.inst(GROWMEMORY, 1, true);
        self.code.push(0);
    }

    fn log_const<T>(&self, opcode: OpCode, x: T)
    where
        T: std::fmt::Debug + std::fmt::Display,
    {
        log_instruction!(
            "{:10}\t{}\t{:?}",
            format!("{:?}", opcode),
            x,
            self.current_stack()
        );
    }
    pub fn i32_const(&mut self, x: i32) {
        self.inst_base(I32CONST, 0, true);
        self.code.encode_i32(x);
        self.log_const(I32CONST, x);
    }
    pub fn i64_const(&mut self, x: i64) {
        self.inst_base(I64CONST, 0, true);
        self.code.encode_i64(x);
        self.log_const(I64CONST, x);
    }
    pub fn f32_const(&mut self, x: f32) {
        self.inst_base(F32CONST, 0, true);
        self.code.encode_f32(x);
        self.log_const(F32CONST, x);
    }
    pub fn f64_const(&mut self, x: f64) {
        self.inst_base(F64CONST, 0, true);
        self.code.encode_f64(x);
        self.log_const(F64CONST, x);
    }

    // TODO: Consider creating unified methods for numerical ops like 'eq' and 'add',
    // passing the ValueType as an argument. Could simplify lowlevel code gen.
    instruction_no_args!(i32_eqz, I32EQZ, 1, true);
    instruction_no_args!(i32_eq, I32EQ, 2, true);
    instruction_no_args!(i32_ne, I32NE, 2, true);
    instruction_no_args!(i32_lt_s, I32LTS, 2, true);
    instruction_no_args!(i32_lt_u, I32LTU, 2, true);
    instruction_no_args!(i32_gt_s, I32GTS, 2, true);
    instruction_no_args!(i32_gt_u, I32GTU, 2, true);
    instruction_no_args!(i32_le_s, I32LES, 2, true);
    instruction_no_args!(i32_le_u, I32LEU, 2, true);
    instruction_no_args!(i32_ge_s, I32GES, 2, true);
    instruction_no_args!(i32_ge_u, I32GEU, 2, true);
    instruction_no_args!(i64_eqz, I64EQZ, 1, true);
    instruction_no_args!(i64_eq, I64EQ, 2, true);
    instruction_no_args!(i64_ne, I64NE, 2, true);
    instruction_no_args!(i64_lt_s, I64LTS, 2, true);
    instruction_no_args!(i64_lt_u, I64LTU, 2, true);
    instruction_no_args!(i64_gt_s, I64GTS, 2, true);
    instruction_no_args!(i64_gt_u, I64GTU, 2, true);
    instruction_no_args!(i64_le_s, I64LES, 2, true);
    instruction_no_args!(i64_le_u, I64LEU, 2, true);
    instruction_no_args!(i64_ge_s, I64GES, 2, true);
    instruction_no_args!(i64_ge_u, I64GEU, 2, true);
    instruction_no_args!(f32_eq, F32EQ, 2, true);
    instruction_no_args!(f32_ne, F32NE, 2, true);
    instruction_no_args!(f32_lt, F32LT, 2, true);
    instruction_no_args!(f32_gt, F32GT, 2, true);
    instruction_no_args!(f32_le, F32LE, 2, true);
    instruction_no_args!(f32_ge, F32GE, 2, true);
    instruction_no_args!(f64_eq, F64EQ, 2, true);
    instruction_no_args!(f64_ne, F64NE, 2, true);
    instruction_no_args!(f64_lt, F64LT, 2, true);
    instruction_no_args!(f64_gt, F64GT, 2, true);
    instruction_no_args!(f64_le, F64LE, 2, true);
    instruction_no_args!(f64_ge, F64GE, 2, true);
    instruction_no_args!(i32_clz, I32CLZ, 1, true);
    instruction_no_args!(i32_ctz, I32CTZ, 1, true);
    instruction_no_args!(i32_popcnt, I32POPCNT, 1, true);
    instruction_no_args!(i32_add, I32ADD, 2, true);
    instruction_no_args!(i32_sub, I32SUB, 2, true);
    instruction_no_args!(i32_mul, I32MUL, 2, true);
    instruction_no_args!(i32_div_s, I32DIVS, 2, true);
    instruction_no_args!(i32_div_u, I32DIVU, 2, true);
    instruction_no_args!(i32_rem_s, I32REMS, 2, true);
    instruction_no_args!(i32_rem_u, I32REMU, 2, true);
    instruction_no_args!(i32_and, I32AND, 2, true);
    instruction_no_args!(i32_or, I32OR, 2, true);
    instruction_no_args!(i32_xor, I32XOR, 2, true);
    instruction_no_args!(i32_shl, I32SHL, 2, true);
    instruction_no_args!(i32_shr_s, I32SHRS, 2, true);
    instruction_no_args!(i32_shr_u, I32SHRU, 2, true);
    instruction_no_args!(i32_rotl, I32ROTL, 2, true);
    instruction_no_args!(i32_rotr, I32ROTR, 2, true);
    instruction_no_args!(i64_clz, I64CLZ, 1, true);
    instruction_no_args!(i64_ctz, I64CTZ, 1, true);
    instruction_no_args!(i64_popcnt, I64POPCNT, 1, true);
    instruction_no_args!(i64_add, I64ADD, 2, true);
    instruction_no_args!(i64_sub, I64SUB, 2, true);
    instruction_no_args!(i64_mul, I64MUL, 2, true);
    instruction_no_args!(i64_div_s, I64DIVS, 2, true);
    instruction_no_args!(i64_div_u, I64DIVU, 2, true);
    instruction_no_args!(i64_rem_s, I64REMS, 2, true);
    instruction_no_args!(i64_rem_u, I64REMU, 2, true);
    instruction_no_args!(i64_and, I64AND, 2, true);
    instruction_no_args!(i64_or, I64OR, 2, true);
    instruction_no_args!(i64_xor, I64XOR, 2, true);
    instruction_no_args!(i64_shl, I64SHL, 2, true);
    instruction_no_args!(i64_shr_s, I64SHRS, 2, true);
    instruction_no_args!(i64_shr_u, I64SHRU, 2, true);
    instruction_no_args!(i64_rotl, I64ROTL, 2, true);
    instruction_no_args!(i64_rotr, I64ROTR, 2, true);
    instruction_no_args!(f32_abs, F32ABS, 1, true);
    instruction_no_args!(f32_neg, F32NEG, 1, true);
    instruction_no_args!(f32_ceil, F32CEIL, 1, true);
    instruction_no_args!(f32_floor, F32FLOOR, 1, true);
    instruction_no_args!(f32_trunc, F32TRUNC, 1, true);
    instruction_no_args!(f32_nearest, F32NEAREST, 1, true);
    instruction_no_args!(f32_sqrt, F32SQRT, 1, true);
    instruction_no_args!(f32_add, F32ADD, 2, true);
    instruction_no_args!(f32_sub, F32SUB, 2, true);
    instruction_no_args!(f32_mul, F32MUL, 2, true);
    instruction_no_args!(f32_div, F32DIV, 2, true);
    instruction_no_args!(f32_min, F32MIN, 2, true);
    instruction_no_args!(f32_max, F32MAX, 2, true);
    instruction_no_args!(f32_copysign, F32COPYSIGN, 2, true);
    instruction_no_args!(f64_abs, F64ABS, 1, true);
    instruction_no_args!(f64_neg, F64NEG, 1, true);
    instruction_no_args!(f64_ceil, F64CEIL, 1, true);
    instruction_no_args!(f64_floor, F64FLOOR, 1, true);
    instruction_no_args!(f64_trunc, F64TRUNC, 1, true);
    instruction_no_args!(f64_nearest, F64NEAREST, 1, true);
    instruction_no_args!(f64_sqrt, F64SQRT, 1, true);
    instruction_no_args!(f64_add, F64ADD, 2, true);
    instruction_no_args!(f64_sub, F64SUB, 2, true);
    instruction_no_args!(f64_mul, F64MUL, 2, true);
    instruction_no_args!(f64_div, F64DIV, 2, true);
    instruction_no_args!(f64_min, F64MIN, 2, true);
    instruction_no_args!(f64_max, F64MAX, 2, true);
    instruction_no_args!(f64_copysign, F64COPYSIGN, 2, true);
    instruction_no_args!(i32_wrap_i64, I32WRAPI64, 1, true);
    instruction_no_args!(i32_trunc_s_f32, I32TRUNCSF32, 1, true);
    instruction_no_args!(i32_trunc_u_f32, I32TRUNCUF32, 1, true);
    instruction_no_args!(i32_trunc_s_f64, I32TRUNCSF64, 1, true);
    instruction_no_args!(i32_trunc_u_f64, I32TRUNCUF64, 1, true);
    instruction_no_args!(i64_extend_s_i32, I64EXTENDSI32, 1, true);
    instruction_no_args!(i64_extend_u_i32, I64EXTENDUI32, 1, true);
    instruction_no_args!(i64_trunc_s_f32, I64TRUNCSF32, 1, true);
    instruction_no_args!(i64_trunc_u_f32, I64TRUNCUF32, 1, true);
    instruction_no_args!(i64_trunc_s_f64, I64TRUNCSF64, 1, true);
    instruction_no_args!(i64_trunc_u_f64, I64TRUNCUF64, 1, true);
    instruction_no_args!(f32_convert_s_i32, F32CONVERTSI32, 1, true);
    instruction_no_args!(f32_convert_u_i32, F32CONVERTUI32, 1, true);
    instruction_no_args!(f32_convert_s_i64, F32CONVERTSI64, 1, true);
    instruction_no_args!(f32_convert_u_i64, F32CONVERTUI64, 1, true);
    instruction_no_args!(f32_demote_f64, F32DEMOTEF64, 1, true);
    instruction_no_args!(f64_convert_s_i32, F64CONVERTSI32, 1, true);
    instruction_no_args!(f64_convert_u_i32, F64CONVERTUI32, 1, true);
    instruction_no_args!(f64_convert_s_i64, F64CONVERTSI64, 1, true);
    instruction_no_args!(f64_convert_u_i64, F64CONVERTUI64, 1, true);
    instruction_no_args!(f64_promote_f32, F64PROMOTEF32, 1, true);
    instruction_no_args!(i32_reinterpret_f32, I32REINTERPRETF32, 1, true);
    instruction_no_args!(i64_reinterpret_f64, I64REINTERPRETF64, 1, true);
    instruction_no_args!(f32_reinterpret_i32, F32REINTERPRETI32, 1, true);
    instruction_no_args!(f64_reinterpret_i64, F64REINTERPRETI64, 1, true);
}