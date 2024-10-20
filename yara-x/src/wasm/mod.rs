/*! WASM runtime

During the compilation process the condition associated to each YARA rule is
translated into WebAssembly (WASM) code. This code is later converted to native
code and executed by [wasmtime](https://wasmtime.dev/), a WASM runtime embedded
in YARA.

For each instance of [`crate::compiler::Rules`] the compiler creates a WASM
module. This WASM module works in close collaboration with YARA's Rust code for
evaluating the rule's conditions. For example, the WASM module exports a
function called `main`, which contains the code that evaluates the conditions
of all the compiled rules. This WASM function is called by YARA at scan time,
and the WASM code calls back the Rust [`rule_match`] function for notifying
YARA about matching rules. The WASM module calls Rust functions in many other
cases, for example when it needs to call YARA built-in functions like
`uint8(...)`, or functions implemented by YARA modules.

WASM and Rust code also share information via WASM global variables and by
sharing memory. For example, the value for YARA's `filesize` keyword is
stored in a WASM global variable that is initialized by Rust code, and read
by WASM code when `filesize` is used in the condition.

# Memory layout

The memory of these WASM modules is organized as follows.

```text
  ┌──────────────────────────┐ 0
  │ Variable #0              │ 8
  │ Variable #1              │ 16
  : ...                      :
  │ Variable #n              │ n * 8
  : ...                      :
  │                          │
  ├──────────────────────────┤ 1024
  │ Field lookup indexes     │
  ├──────────────────────────┤ 2048
  │ Matching rules bitmap    │
  │                          │
  :                          :
  │                          │
  ├──────────────────────────┤ (number of rules / 8) + 1
  │ Matching patterns bitmap │
  │                          │
  :                          :
  │                          │
  └──────────────────────────┘
```

# Field lookup

While evaluating rule condition's, the WASM code needs to obtain from YARA the
values stored in structures, maps and arrays. In order to minimize the number
of calls from WASM to Rust, these field lookups are performed in bulk. For
example, suppose that a YARA module named `some_module` exports a structure
named `some_struct` that has an integer field named `some_int`. For accessing,
that field in a YARA rule you would write `some_module.some_struct.some_int`.
The WASM code for obtaining the value of `some_int` consists in a single call
to the [`lookup_integer`] function. This functions receives a series of field
indexes: the index of `some_module` within the global structure, the index
of `some_struct` within `some_module`, and finally the index of `some_int`,
within `some_struct`. These indexes are stored starting at offset 1024 in
the WASM module's main memory (see "Memory layout") before calling
[`lookup_integer`], while the global variable `lookup_stack_top` says how
many indexes to lookup.

 */
use std::any::{type_name, TypeId};
use std::borrow::Borrow;
use std::mem;

use bitvec::order::Lsb0;
use bitvec::slice::BitSlice;
use bstr::ByteSlice;
use lazy_static::lazy_static;
use linkme::distributed_slice;
use smallvec::{smallvec, SmallVec};
use wasmtime::{
    AsContextMut, Caller, Config, Engine, FuncType, Linker, ValRaw,
};

use yara_x_macros::wasm_export;
use yara_x_parser::types::{Map, TypeValue};

use crate::compiler::{PatternId, RuleId};
use crate::modules::BUILTIN_MODULES;
use crate::scanner::ScanContext;
use crate::wasm::string::{RuntimeString, RuntimeStringWasm};
use crate::LiteralId;

pub(crate) mod builder;
pub(crate) mod string;

/// Offset in module's main memory where the space for loop variables start.
pub(crate) const VARS_STACK_START: i32 = 0;
/// Offset in module's main memory where the space for loop variables end.
pub(crate) const VARS_STACK_END: i32 = VARS_STACK_START + 1024;

/// Offset in module's main memory where the space for lookup indexes start.
pub(crate) const LOOKUP_INDEXES_START: i32 = VARS_STACK_END;
/// Offset in module's main memory where the space for lookup indexes end.
pub(crate) const LOOKUP_INDEXES_END: i32 = LOOKUP_INDEXES_START + 1024;

/// Offset in module's main memory where resides the bitmap that tells if a
/// rule matches or not. This bitmap contains one bit per rule, if the N-th
/// bit is set, it indicates that the rule with RuleId = N matched.
pub(crate) const MATCHING_RULES_BITMAP_BASE: i32 = LOOKUP_INDEXES_END;

/// Global slice that contains an entry for each function that is callable from
/// WASM code. Functions with attributes `#[wasm_export]` and `#[module_export]`
/// are automatically added to this slice. See https://github.com/dtolnay/linkme
/// for details about how `#[distributed_slice]` works.
#[distributed_slice]
pub(crate) static WASM_EXPORTS: [WasmExport] = [..];

/// Type of each entry in [`WASM_EXPORTS`].
pub(crate) struct WasmExport {
    /// Function's name.
    pub name: &'static str,
    /// Function's mangled name. The mangled name contains information about
    /// the function's arguments and return type. For additional details see
    /// [`yara_x_parser::types::MangledFnName`].
    pub mangled_name: &'static str,
    /// True if the function is visible from YARA rules. Functions exported by
    /// modules, as well as built-in functions like uint8, uint16, etc are
    /// public, but many other functions callable from WASM are for internal
    /// use only and therefore are not public.
    pub public: bool,
    /// Path of the module where the function resides. This an absolute path
    /// that includes the crate name (e.g: yara_x::modules::test_proto2)
    pub rust_module_path: &'static str,
    /// Reference to some type that implements the WasmExportedFn trait.
    pub func: &'static (dyn WasmExportedFn + Send + Sync),
}

impl WasmExport {
    /// Returns the fully qualified name for a #[wasm_export] function.
    ///
    /// The fully qualified name includes not only the function's name, but
    /// also the module's name (e.g: `my_module.my_struct.my_func@ii@i`)
    pub fn fully_qualified_mangled_name(&self) -> String {
        for (module_name, module) in BUILTIN_MODULES.iter() {
            if let Some(rust_module_name) = module.rust_module_name {
                if self.rust_module_path.contains(rust_module_name) {
                    return format!("{}.{}", module_name, self.mangled_name);
                }
            }
        }
        self.mangled_name.to_owned()
    }
}

/// Trait implemented for all types that represent a function exported to WASM.
///
/// Implementors of this trait are [`WasmExportedFn0`], [`WasmExportedFn1`],
/// [`WasmExportedFn2`], etc. Each of these types is a generic type that
/// represents all functions with 0, 1, and 2 arguments respectively.
pub(crate) trait WasmExportedFn {
    /// Returns the function that will be passed to [`wasmtime::Func::new`]
    /// while linking the WASM code to this function.
    fn trampoline(&'static self) -> TrampolineFn;

    /// Returns a [`Vec<walrus::ValType>`] with the types of the function's
    /// arguments
    fn walrus_args(&'static self) -> Vec<walrus::ValType>;

    /// Returns a [`Vec<walrus::ValType>`] with the types of the function's
    /// return values.
    fn walrus_results(&'static self) -> Vec<walrus::ValType>;

    /// Returns a [`Vec<wasmtime::ValType>`] with the types of the function's
    /// arguments
    fn wasmtime_args(&'static self) -> Vec<wasmtime::ValType> {
        self.walrus_args().iter().map(walrus_to_wasmtime).collect()
    }

    /// Returns a [`Vec<wasmtime::ValType>`] with the types of the function's
    /// return values.
    fn wasmtime_results(&'static self) -> Vec<wasmtime::ValType> {
        self.walrus_results().iter().map(walrus_to_wasmtime).collect()
    }
}

type TrampolineFn = Box<
    dyn Fn(Caller<'_, ScanContext>, &mut [ValRaw]) -> anyhow::Result<()>
        + Send
        + Sync
        + 'static,
>;

/// Represents an argument passed to a `#[wasm_export]` function.
///
/// The purpose of this type is converting [`wasmtime::ValRaw`] into Rust
/// types (e.g: `i64`, `i32`, `f64`, `f32`, etc)
struct WasmArg(ValRaw);

impl From<ValRaw> for WasmArg {
    fn from(value: ValRaw) -> Self {
        Self(value)
    }
}

impl From<WasmArg> for i64 {
    fn from(value: WasmArg) -> Self {
        value.0.get_i64()
    }
}

impl From<WasmArg> for i32 {
    fn from(value: WasmArg) -> Self {
        value.0.get_i32()
    }
}

impl From<WasmArg> for f64 {
    fn from(value: WasmArg) -> Self {
        f64::from_bits(value.0.get_f64())
    }
}

impl From<WasmArg> for f32 {
    fn from(value: WasmArg) -> Self {
        f32::from_bits(value.0.get_f32())
    }
}

impl From<WasmArg> for LiteralId {
    fn from(value: WasmArg) -> Self {
        LiteralId::from(value.0.get_i32())
    }
}

impl From<WasmArg> for RuntimeString {
    fn from(value: WasmArg) -> Self {
        Self::from_wasm(RuntimeStringWasm::from(value))
    }
}

/// A trait for converting a value into an array of [`wasmtime::ValRaw`]
/// suitable to be passed to WASM code.
///
/// Functions with the `#[wasm_export]` attribute must return a type that
/// implements this trait.
pub(crate) trait ToWasm {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]>;
}

impl ToWasm for () {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![]
    }
}

impl ToWasm for i32 {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![ValRaw::i32(*self)]
    }
}

impl ToWasm for i64 {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![ValRaw::i64(*self)]
    }
}

impl ToWasm for f32 {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![ValRaw::f32(f32::to_bits(*self))]
    }
}

impl ToWasm for f64 {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![ValRaw::f64(f64::to_bits(*self))]
    }
}

impl ToWasm for bool {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![ValRaw::i32(*self as i32)]
    }
}

impl ToWasm for RuntimeString {
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        smallvec![ValRaw::i64(self.as_wasm())]
    }
}

impl<T> ToWasm for MaybeUndef<T>
where
    T: ToWasm + Default,
{
    fn to_wasm(&self) -> SmallVec<[ValRaw; 2]> {
        match self {
            MaybeUndef::Ok(value) => {
                let mut result = value.to_wasm();
                result.push(ValRaw::i32(0));
                result
            }
            MaybeUndef::Undef::<T> => {
                let mut result = T::default().to_wasm();
                result.push(ValRaw::i32(1));
                result
            }
        }
    }
}

/// Return type for functions that may return an undefined value.
pub enum MaybeUndef<T> {
    Ok(T),
    Undef,
}

pub fn walrus_to_wasmtime(ty: &walrus::ValType) -> wasmtime::ValType {
    match ty {
        walrus::ValType::I64 => wasmtime::ValType::I64,
        walrus::ValType::I32 => wasmtime::ValType::I32,
        walrus::ValType::F64 => wasmtime::ValType::F64,
        walrus::ValType::F32 => wasmtime::ValType::F32,
        _ => unreachable!(),
    }
}

#[allow(clippy::if_same_then_else)]
fn type_id_to_walrus(
    type_id: TypeId,
    type_name: &'static str,
) -> &'static [walrus::ValType] {
    if type_id == TypeId::of::<i64>() {
        return &[walrus::ValType::I64];
    } else if type_id == TypeId::of::<i32>() {
        return &[walrus::ValType::I32];
    } else if type_id == TypeId::of::<f64>() {
        return &[walrus::ValType::F64];
    } else if type_id == TypeId::of::<f32>() {
        return &[walrus::ValType::F32];
    } else if type_id == TypeId::of::<LiteralId>() {
        return &[walrus::ValType::I32];
    } else if type_id == TypeId::of::<bool>() {
        return &[walrus::ValType::I32];
    } else if type_id == TypeId::of::<()>() {
        return &[];
    } else if type_id == TypeId::of::<RuntimeString>() {
        return &[walrus::ValType::I64];
    } else if type_id == TypeId::of::<MaybeUndef<()>>() {
        return &[walrus::ValType::I32];
    } else if type_id == TypeId::of::<MaybeUndef<i64>>() {
        return &[walrus::ValType::I64, walrus::ValType::I32];
    } else if type_id == TypeId::of::<MaybeUndef<i32>>() {
        return &[walrus::ValType::I32, walrus::ValType::I32];
    } else if type_id == TypeId::of::<MaybeUndef<f64>>() {
        return &[walrus::ValType::F64, walrus::ValType::I32];
    } else if type_id == TypeId::of::<MaybeUndef<f32>>() {
        return &[walrus::ValType::F32, walrus::ValType::I32];
    } else if type_id == TypeId::of::<MaybeUndef<bool>>() {
        return &[walrus::ValType::I32, walrus::ValType::I32];
    } else if type_id == TypeId::of::<MaybeUndef<RuntimeString>>() {
        return &[walrus::ValType::I64, walrus::ValType::I32];
    }
    panic!("type `{}` can't be an argument or return value", type_name)
}

/// Macro that creates types [`WasmExportedFn0`], [`WasmExportedFn1`], etc,
/// and implements the [`WasmExportedFn`] trait for them.
macro_rules! impl_wasm_exported_fn {
    ($name:ident $($args:ident)*) => {
        pub(super) struct $name <$($args,)* R>
        where
            $($args: 'static,)*
            R: 'static,
        {
            pub target_fn: &'static (dyn Fn(Caller<'_, ScanContext>, $($args),*) -> R
                          + Send
                          + Sync
                          + 'static),
        }

        impl<$($args,)* R> WasmExportedFn for $name<$($args,)* R>
        where
            $($args: From<WasmArg>,)*
            R: ToWasm,
        {
            #[allow(unused_mut)]
            fn walrus_args(&'static self) -> Vec<walrus::ValType> {
                let mut result = Vec::new();
                $(
                    result.extend_from_slice(type_id_to_walrus(
                        TypeId::of::<$args>(),
                        type_name::<$args>(),
                    ));
                )*
                result
            }

            fn walrus_results(&'static self) -> Vec<walrus::ValType> {
                Vec::from(type_id_to_walrus(TypeId::of::<R>(), type_name::<R>()))
            }

            #[allow(unused_assignments)]
            #[allow(unused_variables)]
            #[allow(non_snake_case)]
            #[allow(unused_mut)]
            fn trampoline(&'static self) -> TrampolineFn {
                Box::new(
                    |caller: Caller<'_, ScanContext>,
                     args_and_results: &mut [ValRaw]|
                     -> anyhow::Result<()> {
                        let mut i = 0;
                        $(
                            let $args = WasmArg::from(args_and_results[i].clone()).into();
                            i += 1;
                        )*

                        let result = (self.target_fn)(caller, $($args),*);
                        let result = result.to_wasm();

                        let result_slice = result.as_slice();
                        let num_results = result_slice.len();

                        args_and_results[0..num_results].clone_from_slice(result_slice);
                        anyhow::Ok(())
                    },
                )
            }
        }
    };
}

// Generate multiple structures implementing the WasmExportedFn trait,
// each for a different number of arguments. The WasmExportedFn0 is a generic
// type that represents all exported functions that have no arguments,
// WasmExportedFn1 represents functions with 1 argument, and so on.
impl_wasm_exported_fn!(WasmExportedFn0);
impl_wasm_exported_fn!(WasmExportedFn1 A1);
impl_wasm_exported_fn!(WasmExportedFn2 A1 A2);
impl_wasm_exported_fn!(WasmExportedFn3 A1 A2 A3);

/// Table with functions and variables used by the WASM module.
///
/// The WASM module generated for evaluating rule conditions needs to
/// call back to YARA for multiple tasks. For example, it calls YARA for
/// reporting rule matches, for asking if a pattern matches at a given offset,
/// for executing functions like `uint32()`, etc.
///
/// This table contains the [`FunctionId`] for such functions, which are
/// imported by the WASM module and implemented by YARA. It also
/// contains the definition of some variables used by the module.
#[derive(Clone)]
pub(crate) struct WasmSymbols {
    /// The WASM module's main memory.
    pub main_memory: walrus::MemoryId,

    pub lookup_start: walrus::GlobalId,
    pub lookup_stack_top: walrus::GlobalId,

    /// Global variable that contains the offset within the module's main
    /// memory where resides the bitmap that indicates if a pattern matches
    /// or not.
    pub matching_patterns_bitmap_base: walrus::GlobalId,

    /// Global variable that contains the value for `filesize`.
    pub filesize: walrus::GlobalId,

    /// Local variables used for temporary storage.
    pub i64_tmp: walrus::LocalId,
    pub i32_tmp: walrus::LocalId,
}

lazy_static! {
    pub(crate) static ref CONFIG: Config = {
        let mut config = Config::default();
        config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);
        config
    };
    pub(crate) static ref ENGINE: Engine = Engine::new(&CONFIG).unwrap();
    pub(crate) static ref LINKER: Linker<ScanContext<'static>> = new_linker();
}

pub(crate) fn new_linker<'r>() -> Linker<ScanContext<'r>> {
    let mut linker = Linker::<ScanContext<'r>>::new(&ENGINE);
    for export in WASM_EXPORTS {
        let func_type = FuncType::new(
            export.func.wasmtime_args(),
            export.func.wasmtime_results(),
        );
        // Using `func_new_unchecked` instead of `func_new` makes function
        // calls from WASM to Rust around 3x faster.
        unsafe {
            linker
                .func_new_unchecked(
                    export.rust_module_path,
                    export.fully_qualified_mangled_name().as_str(),
                    func_type,
                    export.func.trampoline(),
                )
                .unwrap();
        }
    }

    linker
}

/// Invoked from WASM to notify when a rule matches.
#[wasm_export]
pub(crate) fn rule_match(
    mut caller: Caller<'_, ScanContext>,
    rule_id: RuleId,
) {
    let mut store_ctx = caller.as_context_mut();

    let main_mem =
        store_ctx.data_mut().main_memory.unwrap().data_mut(store_ctx);

    let bits = BitSlice::<u8, Lsb0>::from_slice_mut(
        &mut main_mem[MATCHING_RULES_BITMAP_BASE as usize..],
    );

    // The RuleId-th bit in the `rule_matches` bit vector is set to 1.
    bits.set(rule_id as usize, true);

    caller.as_context_mut().data_mut().rules_matching.push(rule_id);
}

/// Invoked from WASM to ask whether a pattern matches at a given file
/// offset.
///
/// Returns 1 if the pattern identified by `pattern_id` matches at `offset`,
/// or 0 if otherwise.
#[wasm_export]
pub(crate) fn is_pat_match_at(
    _caller: Caller<'_, ScanContext>,
    _pattern_id: PatternId,
    _offset: i64,
) -> bool {
    // TODO
    false
}

/// Invoked from WASM to ask whether a pattern at some offset within
/// given range.
///
/// Returns 1 if the pattern identified by `pattern_id` matches at some offset
/// in the range [`lower_bound`, `upper_bound`].
#[wasm_export]
pub(crate) fn is_pat_match_in(
    _caller: Caller<'_, ScanContext>,
    _pattern_id: PatternId,
    _lower_bound: i64,
    _upper_bound: i64,
) -> bool {
    // TODO
    false
}

/// Given some local variable containing an array, returns the length of the
/// array. The local variable is an index within `vars_stack`.
///
/// # Panics
///
/// If the variable doesn't exist or is not an array.
#[wasm_export]
pub(crate) fn array_len(mut caller: Caller<'_, ScanContext>, var: i32) -> i64 {
    let ctx = caller.data_mut();

    let len =
        ctx.vars_stack.get(var as usize).unwrap().as_array().unwrap().len();

    len as i64
}

/// Given some local variable containing a map, returns the length of the
/// map. The local variable is an index within `vars_stack`.
///
/// # Panics
///
/// If the variable doesn't exist or is not a map.
#[wasm_export]
pub(crate) fn map_len(mut caller: Caller<'_, ScanContext>, var: i32) -> i64 {
    let ctx = caller.data_mut();

    let len =
        ctx.vars_stack.get(var as usize).unwrap().as_map().unwrap().len();

    len as i64
}

macro_rules! lookup_common {
    ($caller:ident, $type_value:ident, $code:block) => {{
        let lookup_start = $caller
            .data()
            .lookup_start
            .unwrap()
            .get(&mut $caller.as_context_mut())
            .i32()
            .unwrap();

        let lookup_stack_top = $caller
            .data()
            .lookup_stack_top
            .unwrap()
            .get(&mut $caller.as_context_mut())
            .i32()
            .unwrap();

        let mut store_ctx = $caller.as_context_mut();

        let lookup_stack_ptr =
            store_ctx.data_mut().main_memory.unwrap().data_ptr(&mut store_ctx);

        let lookup_stack = unsafe {
            std::slice::from_raw_parts::<i32>(
                lookup_stack_ptr.offset(LOOKUP_INDEXES_START as isize)
                    as *const i32,
                lookup_stack_top as usize,
            )
        };

        let $type_value = if lookup_stack.len() > 0 {
            let mut structure = if let Some(current_structure) =
                &store_ctx.data().current_struct
            {
                current_structure.as_ref()
            } else if lookup_start != -1 {
                let var =
                    &store_ctx.data().vars_stack[lookup_start as usize];

                if let TypeValue::Struct(s) = var {
                    s
                } else {
                    unreachable!(
                        "expecting struct, got `{:?}` at variable with index {}",
                        var, lookup_start)
                }
            } else {
                &store_ctx.data().root_struct
            };

            let mut final_field = None;

            for field_index in lookup_stack {
                let field =
                    structure.field_by_index(*field_index as usize).unwrap();
                final_field = Some(field);
                if let TypeValue::Struct(s) = &field.type_value {
                    structure = &s
                }
            }

            &final_field.unwrap().type_value
        } else if lookup_start != -1 {
            &store_ctx.data().vars_stack[lookup_start as usize]
        } else {
            unreachable!();
        };

        let result = $code;

        $caller.data_mut().current_struct = None;

        result
    }};
}

#[wasm_export]
pub(crate) fn lookup_string(
    mut caller: Caller<'_, ScanContext>,
) -> MaybeUndef<RuntimeString> {
    lookup_common!(caller, type_value, {
        match type_value {
            TypeValue::String(Some(value)) => {
                let value = value.to_owned();
                MaybeUndef::Ok(RuntimeString::Owned(
                    caller.data_mut().string_pool.get_or_intern(value),
                ))
            }
            TypeValue::String(None) => MaybeUndef::Undef,
            _ => unreachable!(),
        }
    })
}

#[wasm_export]
pub(crate) fn lookup_value(mut caller: Caller<'_, ScanContext>, var: i32) {
    let value = lookup_common!(caller, type_value, { type_value.clone() });
    let index = var as usize;

    let vars = &mut caller.data_mut().vars_stack;

    if vars.len() <= index {
        vars.resize(index + 1, TypeValue::Unknown);
    }

    vars[index] = value;
}

macro_rules! gen_lookup_fn {
    ($name:ident, $return_type:ty, $type:path) => {
        #[wasm_export]
        pub(crate) fn $name(
            mut caller: Caller<'_, ScanContext>,
        ) -> MaybeUndef<$return_type> {
            lookup_common!(caller, type_value, {
                if let $type(Some(value)) = type_value {
                    MaybeUndef::Ok(*value as $return_type)
                } else {
                    MaybeUndef::Undef
                }
            })
        }
    };
}

gen_lookup_fn!(lookup_integer, i64, TypeValue::Integer);
gen_lookup_fn!(lookup_float, f64, TypeValue::Float);
gen_lookup_fn!(lookup_bool, bool, TypeValue::Bool);

macro_rules! gen_array_lookup_fn {
    ($name:ident, $fn:ident, $return_type:ty) => {
        #[wasm_export]
        pub(crate) fn $name(
            mut caller: Caller<'_, ScanContext>,
            index: i64,
            var: i32,
        ) -> MaybeUndef<$return_type> {
            // TODO: decide what to to with this. It looks like are not going to need
            // to store integer, floats nor bools in host-side variables.
            assert_eq!(var, -1);

            let array = lookup_common!(caller, type_value, {
                type_value.as_array().unwrap()
            });

            let array = array.$fn();

            if let Some(value) = array.get(index as usize) {
                MaybeUndef::Ok(*value as $return_type)
            } else {
                MaybeUndef::Undef
            }
        }
    };
}

gen_array_lookup_fn!(array_lookup_integer, as_integer_array, i64);
gen_array_lookup_fn!(array_lookup_float, as_float_array, f64);
gen_array_lookup_fn!(array_lookup_bool, as_bool_array, bool);

#[wasm_export]
pub(crate) fn array_lookup_string(
    mut caller: Caller<'_, ScanContext>,
    index: i64,
    var: i32,
) -> MaybeUndef<RuntimeString> {
    // TODO: decide what to to with this. It looks like are not going to need
    // to store strings in host-side variables.
    assert_eq!(var, -1);

    let array =
        lookup_common!(caller, type_value, { type_value.as_array().unwrap() });

    let array = array.as_string_array();

    if let Some(string) = array.get(index as usize) {
        MaybeUndef::Ok(RuntimeString::Owned(
            caller.data_mut().string_pool.get_or_intern(string.as_bstr()),
        ))
    } else {
        MaybeUndef::Undef
    }
}

#[wasm_export]
pub(crate) fn array_lookup_struct(
    mut caller: Caller<'_, ScanContext>,
    index: i64,
    var: i32,
) -> MaybeUndef<()> {
    let array =
        lookup_common!(caller, type_value, { type_value.as_array().unwrap() });

    let array = array.as_struct_array();

    if let Some(s) = array.get(index as usize) {
        caller.data_mut().current_struct = Some(s.clone());

        if var != -1 {
            let index = var as usize;
            let vars = &mut caller.data_mut().vars_stack;

            if vars.len() <= index {
                vars.resize(index + 1, TypeValue::Unknown);
            }

            vars[index] = TypeValue::Struct(s.clone());
        }

        MaybeUndef::Ok(())
    } else {
        MaybeUndef::Undef
    }
}

macro_rules! gen_map_string_key_lookup_fn {
    ($name:ident, $return_type:ty, $type:path) => {
        #[wasm_export]
        pub(crate) fn $name(
            mut caller: Caller<'_, ScanContext>,
            key: RuntimeString,
        ) -> MaybeUndef<$return_type> {
            let map = lookup_common!(caller, type_value, {
                type_value.as_map().unwrap()
            });

            let key = key.as_bstr(caller.data());

            let value = match map.borrow() {
                Map::StringKeys { map, .. } => map.get(key),
                _ => unreachable!(),
            };

            if let Some($type(Some(value))) = value {
                MaybeUndef::Ok(*value as $return_type)
            } else {
                MaybeUndef::Undef
            }
        }
    };
}

macro_rules! gen_map_integer_key_lookup_fn {
    ($name:ident, $return_type:ty, $type:path) => {
        #[wasm_export]
        pub(crate) fn $name(
            mut caller: Caller<'_, ScanContext>,
            key: i64,
        ) -> MaybeUndef<$return_type> {
            let map = lookup_common!(caller, type_value, {
                type_value.as_map().unwrap()
            });

            let value = match map.borrow() {
                Map::IntegerKeys { map, .. } => map.get(&key),
                _ => unreachable!(),
            };

            if let Some($type(Some(value))) = value {
                MaybeUndef::Ok(*value as $return_type)
            } else {
                MaybeUndef::Undef
            }
        }
    };
}

#[rustfmt::skip]
gen_map_string_key_lookup_fn!(
    map_lookup_string_integer,
    i64,
    TypeValue::Integer
);

#[rustfmt::skip]
gen_map_string_key_lookup_fn!(
    map_lookup_string_float,
    f64,
    TypeValue::Float
);

#[rustfmt::skip]
gen_map_string_key_lookup_fn!(
    map_lookup_string_bool,
    i32,
    TypeValue::Bool
);

#[rustfmt::skip]
gen_map_integer_key_lookup_fn!(
    map_lookup_integer_integer,
    i64,
    TypeValue::Integer
);

#[rustfmt::skip]
gen_map_integer_key_lookup_fn!(
    map_lookup_integer_float,
    f64,
    TypeValue::Float
);

#[rustfmt::skip]
gen_map_integer_key_lookup_fn!(
    map_lookup_integer_bool,
    i32,
    TypeValue::Bool
);

#[wasm_export]
pub(crate) fn map_lookup_integer_string(
    mut caller: Caller<'_, ScanContext>,
    key: i64,
) -> MaybeUndef<RuntimeString> {
    let map =
        lookup_common!(caller, type_value, { type_value.as_map().unwrap() });

    let value = match map.borrow() {
        Map::IntegerKeys { map, .. } => map.get(&key),
        _ => unreachable!(),
    };

    if let Some(value) = value {
        MaybeUndef::Ok(RuntimeString::Owned(
            caller
                .data_mut()
                .string_pool
                .get_or_intern(value.as_bstr().unwrap()),
        ))
    } else {
        MaybeUndef::Undef
    }
}

#[wasm_export]
pub(crate) fn map_lookup_string_string(
    mut caller: Caller<'_, ScanContext>,
    key: RuntimeString,
) -> MaybeUndef<RuntimeString> {
    let map =
        lookup_common!(caller, type_value, { type_value.as_map().unwrap() });

    let key_bstr = key.as_bstr(caller.data());

    let type_value = match map.borrow() {
        Map::StringKeys { map, .. } => map.get(key_bstr),
        _ => unreachable!(),
    };

    if let Some(type_value) = type_value {
        MaybeUndef::Ok(RuntimeString::Owned(
            caller
                .data_mut()
                .string_pool
                .get_or_intern(type_value.as_bstr().unwrap()),
        ))
    } else {
        MaybeUndef::Undef
    }
}

#[wasm_export]
pub(crate) fn map_lookup_integer_struct(
    mut caller: Caller<'_, ScanContext>,
    key: i64,
) -> MaybeUndef<()> {
    let map = lookup_common!(caller, value, {
        match value {
            TypeValue::Map(map) => map.clone(),
            _ => unreachable!(),
        }
    });

    let value = match map.borrow() {
        Map::IntegerKeys { map, .. } => map.get(&key),
        _ => unreachable!(),
    };

    if let Some(value) = value {
        if let TypeValue::Struct(s) = value {
            caller.data_mut().current_struct = Some(s.clone());
            MaybeUndef::Ok(())
        } else {
            unreachable!()
        }
    } else {
        MaybeUndef::Undef
    }
}

#[wasm_export]
pub(crate) fn map_lookup_string_struct(
    mut caller: Caller<'_, ScanContext>,
    key: RuntimeString,
) -> MaybeUndef<()> {
    let map = lookup_common!(caller, value, {
        match value {
            TypeValue::Map(map) => map.clone(),
            _ => unreachable!(),
        }
    });

    let key_bstr = key.as_bstr(caller.data());

    let value = match map.borrow() {
        Map::StringKeys { map, .. } => map.get(key_bstr),
        _ => unreachable!(),
    };

    if let Some(value) = value {
        if let TypeValue::Struct(s) = value {
            caller.data_mut().current_struct = Some(s.clone());
            MaybeUndef::Ok(())
        } else {
            unreachable!()
        }
    } else {
        MaybeUndef::Undef
    }
}

macro_rules! gen_str_cmp_fn {
    ($name:ident, $op:tt) => {
        #[wasm_export]
        pub(crate) fn $name(
            caller: Caller<'_, ScanContext>,
            lhs: RuntimeString,
            rhs: RuntimeString,
        ) -> bool {
            lhs.$op(&rhs, caller.data())
        }
    };
}

gen_str_cmp_fn!(str_eq, eq);
gen_str_cmp_fn!(str_ne, ne);
gen_str_cmp_fn!(str_lt, lt);
gen_str_cmp_fn!(str_gt, gt);
gen_str_cmp_fn!(str_le, le);
gen_str_cmp_fn!(str_ge, ge);

macro_rules! gen_str_op_fn {
    ($name:ident, $op:tt, $case_insensitive:literal) => {
        #[wasm_export]
        pub(crate) fn $name(
            caller: Caller<'_, ScanContext>,
            lhs: RuntimeString,
            rhs: RuntimeString,
        ) -> bool {
            lhs.$op(&rhs, caller.data(), $case_insensitive)
        }
    };
}

gen_str_op_fn!(str_contains, contains, false);
gen_str_op_fn!(str_startswith, starts_with, false);
gen_str_op_fn!(str_endswith, ends_with, false);
gen_str_op_fn!(str_icontains, contains, true);
gen_str_op_fn!(str_istartswith, starts_with, true);
gen_str_op_fn!(str_iendswith, ends_with, true);
gen_str_op_fn!(str_iequals, equals, true);

#[wasm_export]
pub(crate) fn str_len(
    caller: Caller<'_, ScanContext>,
    s: RuntimeString,
) -> i64 {
    s.len(caller.data()) as i64
}

macro_rules! gen_uint_fn {
    ($name:ident, $return_type:ty, $from_fn:ident) => {
        #[wasm_export(public = true)]
        pub(crate) fn $name(
            caller: Caller<'_, ScanContext>,
            offset: i64,
        ) -> MaybeUndef<i64> {
            if let Ok(offset) = usize::try_from(offset) {
                caller
                    .data()
                    .scanned_data()
                    .get(offset..offset + mem::size_of::<$return_type>())
                    .map_or(MaybeUndef::Undef, |bytes| {
                        let value = <$return_type>::$from_fn(
                            bytes.try_into().unwrap(),
                        );
                        MaybeUndef::Ok(value as i64)
                    })
            } else {
                MaybeUndef::Undef
            }
        }
    };
}

gen_uint_fn!(uint8, u8, from_le_bytes);
gen_uint_fn!(uint16, u16, from_le_bytes);
gen_uint_fn!(uint32, u32, from_le_bytes);
gen_uint_fn!(uint64, u64, from_le_bytes);
gen_uint_fn!(uint8be, u8, from_be_bytes);
gen_uint_fn!(uint16be, u16, from_be_bytes);
gen_uint_fn!(uint32be, u32, from_be_bytes);
gen_uint_fn!(uint64be, u64, from_be_bytes);

#[cfg(test)]
mod tests {
    use crate::wasm::{MaybeUndef, ToWasm};

    #[test]
    fn wasm_result_conversion() {
        let w = 1_i64.to_wasm();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].get_i64(), 1);

        let w = 1_i32.to_wasm();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].get_i32(), 1);

        let w = MaybeUndef::<i64>::Ok(2).to_wasm();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].get_i64(), 2);
        assert_eq!(w[1].get_i32(), 0);

        let w = MaybeUndef::<i64>::Undef.to_wasm();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].get_i64(), 0);
        assert_eq!(w[1].get_i32(), 1);

        let w = MaybeUndef::<i32>::Ok(2).to_wasm();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].get_i64(), 2);
        assert_eq!(w[1].get_i32(), 0);

        let w = MaybeUndef::<i32>::Undef.to_wasm();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].get_i32(), 0);
        assert_eq!(w[1].get_i32(), 1);

        let w = MaybeUndef::<()>::Ok(()).to_wasm();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].get_i32(), 0);

        let w = MaybeUndef::<()>::Undef.to_wasm();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].get_i32(), 1);
    }
}
