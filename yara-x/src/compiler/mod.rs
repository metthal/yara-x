/*! Compiles YARA source code into binary form.

YARA rules must be compiled before they can be used for scanning data. This
module implements the YARA compiler.
*/
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::{fmt, mem};
use walrus::ir::InstrSeqId;
use walrus::{FunctionId, Module, ValType};

use yara_x_parser::ast;
use yara_x_parser::ast::*;
use yara_x_parser::report::ReportBuilder;
use yara_x_parser::types::{Func, FuncSignature};
use yara_x_parser::types::{Struct, TypeValue};
use yara_x_parser::warnings::Warning;
use yara_x_parser::{ErrorInfo as ParserError, Parser, SourceCode};

use crate::compiler::emit::emit_rule_code;
use crate::compiler::semcheck::{semcheck, warn_if_not_bool};
use crate::string_pool::{BStringPool, StringPool};
use crate::symbols::{
    StackedSymbolTable, Symbol, SymbolKind, SymbolLookup, SymbolTable,
};

use crate::wasm;
use crate::wasm::builder::ModuleBuilder;
use crate::wasm::{WasmSymbols, WASM_EXPORTS};

#[doc(inline)]
pub use crate::compiler::errors::*;
use crate::modules::BUILTIN_MODULES;

mod emit;
mod errors;
mod semcheck;

#[cfg(test)]
mod tests;

/// Takes YARA source code and produces compiled [`Rules`].
pub struct Compiler<'a> {
    /// Used for generating error and warning reports.
    report_builder: ReportBuilder,

    /// The main symbol table used by the compiler.
    symbol_table: StackedSymbolTable<'a>,

    /// Pool that contains all the identifiers used in the rules. Each
    /// identifier appears only once, even if they are used by multiple
    /// rules. For example, the pool contains a single copy of the common
    /// identifier `$a`. Each identifier have an unique 32-bits [`IdentId`]
    /// that can be used for retrieving the identifier from the pool.
    ident_pool: StringPool<IdentId>,

    /// Similar to `ident_pool` but for string literals found in the source
    /// code. As literal strings in YARA can contain arbitrary bytes, a pool
    /// capable of storing [`bstr::BString`] must be used, the [`String`] type
    /// only accepts valid UTF-8.
    lit_pool: BStringPool<LiteralId>,

    /// Builder for creating the WebAssembly module that contains the code
    /// for all rule conditions.
    wasm_mod: ModuleBuilder,

    /// A vector with all the rules that has been compiled. A [`RuleId`] is
    /// an index in this vector.
    rules: Vec<Rule>,

    /// A vector with all the patterns from all the rules. A [`PatternId`]
    /// is an index in this vector.
    patterns: Vec<Pattern>,

    /// Vector with the names of all the imported modules. The vector contains
    /// the [`IdentId`] corresponding to the module's identifier.
    imported_modules: Vec<IdentId>,

    /// Structure where each field corresponds to a module imported by the
    /// rules. The value of each field is the structure that describes the
    /// module.
    modules_struct: Struct,

    /// Warnings generated while compiling the rules.
    warnings: Vec<Warning>,
}

impl<'a> Compiler<'a> {
    /// Creates a new YARA compiler.
    pub fn new() -> Self {
        let mut symbol_table = StackedSymbolTable::new();

        // Add symbols for built-in functions like uint8, uint16, etc.
        let builtin_functions = symbol_table.push_new();

        for export in WASM_EXPORTS.iter().filter(|e| e.public) {
            let func = Rc::new(Func::with_signature(FuncSignature::from(
                export.mangled_name.to_string(),
            )));

            let mut symbol = Symbol::new(TypeValue::Func(func.clone()));
            symbol.kind = SymbolKind::Func(func);

            builtin_functions.borrow_mut().insert(export.name, symbol);
        }

        Self {
            symbol_table,
            warnings: Vec::new(),
            rules: Vec::new(),
            patterns: Vec::new(),
            imported_modules: Vec::new(),
            modules_struct: Struct::new(),
            report_builder: ReportBuilder::new(),
            ident_pool: StringPool::new(),
            lit_pool: BStringPool::new(),
            wasm_mod: ModuleBuilder::new(),
        }
    }

    /// Specifies whether the compiler should produce colorful error messages.
    ///
    /// Colorized error messages contain ANSI escape sequences that make them
    /// look nicer on compatible consoles. The default setting is `false`.
    pub fn colorize_errors(mut self, b: bool) -> Self {
        self.report_builder.with_colors(b);
        self
    }

    /// Adds a YARA source code to be compiled.
    ///
    /// This function can be called multiple times.
    pub fn add_source<'src, S>(mut self, src: S) -> Result<Self, Error>
    where
        S: Into<SourceCode<'src>>,
    {
        // Convert `src` into an instance of `SourceCode` if it is something
        // else, like a &str.
        let src = src.into();

        // Parse the source code and build the Abstract Syntax Tree.
        let mut ast = Parser::new()
            .set_report_builder(&self.report_builder)
            .build_ast(src.clone())?;

        // Transfer the warnings generated by the parser to the compiler
        self.warnings.append(&mut ast.warnings);

        for ns in ast.namespaces.iter_mut() {
            // Create the symbol table that will contain the symbols defined
            // in this namespace, like modules and rules.
            let namespace_symbols = self.symbol_table.push_new();

            // Process import statements. Checks that all imported modules
            // actually exist, and raise warnings in case of duplicated
            // imports. For each module add a symbol to the current namespace.
            self.process_imports(&src, &ns.imports, &namespace_symbols)?;

            // Iterate over the list of declared rules and verify that their
            // conditions are semantically valid. For each rule add a symbol
            // to the current namespace.
            for rule in ns.rules.iter_mut() {
                self.process_rule(rule, &src, &namespace_symbols)?;
            }

            // Remove the symbol table for the current namespace. The next
            // namespace can't access symbols defined by this namespace.
            self.symbol_table.pop();
        }

        Ok(self)
    }

    /// Builds the source code previously added to the compiler.
    ///
    /// This function consumes the compiler and returns an instance of
    /// [`Rules`].
    pub fn build(self) -> Result<Rules, Error> {
        // Finish building the WASM module.
        let mut wasm_mod = self.wasm_mod.build();

        // Compile the WASM module for the current platform. This panics
        // if the WASM code is invalid, which should not happen as the code is
        // emitted by YARA itself. If this ever happens is probably because
        // wrong WASM code is being emitted.
        let compiled_wasm_mod = wasmtime::Module::from_binary(
            &crate::wasm::ENGINE,
            wasm_mod.emit_wasm().as_slice(),
        )
        .expect("WASM module is not valid");

        Ok(Rules {
            compiled_wasm_mod,
            wasm_mod,
            ident_pool: self.ident_pool,
            lit_pool: self.lit_pool,
            imported_modules: self.imported_modules,
            patterns: self.patterns,
            rules: self.rules,
        })
    }

    /// Emits a `.wasm` file with the WASM module generated by the compiler.
    ///
    /// This file can be inspected and converted to WASM text format by using
    /// third-party [tooling](https://github.com/WebAssembly/wabt). This is
    /// useful for debugging issues with incorrectly emitted WASM code.
    pub fn emit_wasm_file<P>(self, path: P) -> Result<(), Error>
    where
        P: AsRef<Path>,
    {
        let mut wasm_mod = self.wasm_mod.build();
        Ok(wasm_mod.emit_wasm_file(path)?)
    }
}

impl<'a> Compiler<'a> {
    fn process_rule(
        &mut self,
        rule: &mut ast::Rule,
        src: &SourceCode,
        namespace_symbols: &Rc<RefCell<SymbolTable>>,
    ) -> Result<(), Error> {
        // Create array with pairs (IdentId, PatternId) that describe
        // the patterns in a compiled rule.
        let pairs = if let Some(patterns) = &rule.patterns {
            let mut pairs = Vec::with_capacity(patterns.len());
            for pattern in patterns {
                let ident_id = self
                    .ident_pool
                    .get_or_intern(pattern.identifier().as_str());

                // PatternId is the index of the pattern in
                // `self.patterns`.
                let pattern_id = self.patterns.len() as PatternId;

                self.patterns.push(Pattern {});

                pairs.push((ident_id, pattern_id));
            }
            pairs
        } else {
            Vec::new()
        };

        let rule_id = self.rules.len() as RuleId;

        self.rules.push(Rule {
            ident: self.ident_pool.get_or_intern(rule.identifier.as_str()),
            patterns: pairs,
        });

        let mut ctx = Context {
            src,
            current_struct: None,
            current_signature: None,
            symbol_table: &mut self.symbol_table,
            ident_pool: &mut self.ident_pool,
            lit_pool: &mut self.lit_pool,
            report_builder: &self.report_builder,
            current_rule: self.rules.last().unwrap(),
            wasm_symbols: self.wasm_mod.wasm_symbols(),
            wasm_funcs: &self.wasm_mod.wasm_funcs,
            warnings: &mut self.warnings,
            exception_handler_stack: Vec::new(),
            vars_stack_top: 0,
            lookup_start: None,
            lookup_stack: VecDeque::new(),
        };

        // Insert symbol of type boolean for the rule. This allows
        // other rules to make reference to this one.
        let mut symbol = Symbol::new(TypeValue::Bool(None));

        symbol.kind = SymbolKind::Rule(rule_id);

        namespace_symbols
            .as_ref()
            .borrow_mut()
            .insert(rule.identifier.as_str(), symbol);

        // Verify that the rule's condition is semantically valid. This
        // traverses the condition's AST recursively. The condition can
        // be an expression returning a bool, integer, float or string.
        // Integer, float and string results are casted to boolean.
        semcheck!(
            &mut ctx,
            Type::Bool | Type::Integer | Type::Float | Type::String,
            &mut rule.condition
        )?;

        // If the condition's result is not a boolean and must be casted,
        // raise a warning about it.
        warn_if_not_bool(&mut ctx, &rule.condition);

        // Emit the code for the rule's condition.
        emit_rule_code(
            &mut ctx,
            &mut self.wasm_mod.main_fn.func_body(),
            rule_id,
            rule,
        );

        // After emitting the whole condition, the stack should be empty.
        assert_eq!(ctx.vars_stack_top, 0);

        Ok(())
    }

    fn process_imports(
        &mut self,
        src: &SourceCode,
        imports: &[Import],
        namespace_symbols: &Rc<RefCell<SymbolTable>>,
    ) -> Result<(), Error> {
        // Iterate over the list of imported modules.
        for import in imports.iter() {
            // Does the imported module actually exist? ...
            if let Some(module) =
                BUILTIN_MODULES.get(import.module_name.as_str())
            {
                // ... if yes, add the module to the list of imported modules
                // and the symbol table.
                let module_name = import.module_name.as_str();

                self.imported_modules
                    .push(self.ident_pool.get_or_intern(module_name));

                // Create the structure that describes the module.
                let mut module_struct = Struct::from_proto_descriptor_and_msg(
                    &module.root_struct_descriptor,
                    None,
                    true,
                );

                // Does the YARA module has an associated Rust module? If
                // yes, search for functions exported by the module.
                if let Some(mod_name) = module.rust_module_name {
                    // This map will contain all the functions exported by the
                    // YARA module. Keys are the function names, and values
                    // are `Func` objects.
                    let mut functions: FxHashMap<&'static str, Func> =
                        FxHashMap::default();

                    // Iterate over public functions in WASM_EXPORTS looking
                    // for those that were exported by the current YARA module.
                    // Add them to `functions` map, or update the `Func` object
                    // an additional signature if the function is overloaded.
                    for export in WASM_EXPORTS.iter().filter(|e| e.public) {
                        if export.rust_module_path.contains(mod_name) {
                            let signature = FuncSignature::from(format!(
                                "{}.{}",
                                module_name, export.mangled_name
                            ));
                            // If the function was already present in the map
                            // is because it has multiple signatures. If that's
                            // the case, add more signatures to the existing
                            // `Func` object.
                            if let Some(function) =
                                functions.get_mut(export.name)
                            {
                                function.add_signature(signature)
                            } else {
                                functions.insert(
                                    export.name,
                                    Func::with_signature(signature),
                                );
                            }
                        }
                    }

                    // Insert the functions in the module's struct.
                    for (name, export) in functions.drain() {
                        module_struct
                            .add_field(name, TypeValue::Func(Rc::new(export)));
                    }
                }

                let module_struct = TypeValue::Struct(Rc::new(module_struct));

                // Insert the module in the struct that contains all imported
                // modules. This struct contains all modules imported, from
                // all namespaces.
                self.modules_struct
                    .add_field(module_name, module_struct.clone());

                // Create a symbol for the module and insert it in the symbol
                // table for this namespace.
                let mut symbol = Symbol::new(module_struct);

                symbol.kind = SymbolKind::FieldIndex(
                    self.modules_struct
                        .field_by_name(module_name)
                        .unwrap()
                        .index as i32,
                );

                // Insert the symbol in the symbol table for the current
                // namespace
                namespace_symbols
                    .as_ref()
                    .borrow_mut()
                    .insert(module_name, symbol);
            } else {
                // ... if no, that's an error.
                return Err(Error::CompileError(
                    CompileError::unknown_module(
                        &self.report_builder,
                        src,
                        import.module_name.to_string(),
                        import.span(),
                    ),
                ));
            }
        }

        Ok(())
    }
}

impl fmt::Debug for Compiler<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Compiler")
    }
}

impl Default for Compiler<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// ID associated to each identifier in the identifiers pool.
#[derive(PartialEq, Debug, Copy, Clone)]
pub(crate) struct IdentId(u32);

impl From<u32> for IdentId {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<IdentId> for u32 {
    fn from(v: IdentId) -> Self {
        v.0
    }
}

/// ID associated to each literal string in the literals pool.
#[derive(PartialEq, Debug, Copy, Clone)]
pub(crate) struct LiteralId(u32);

impl From<i32> for LiteralId {
    fn from(v: i32) -> Self {
        Self(v as u32)
    }
}

impl From<u32> for LiteralId {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<LiteralId> for u32 {
    fn from(v: LiteralId) -> Self {
        v.0
    }
}

impl From<LiteralId> for i64 {
    fn from(v: LiteralId) -> Self {
        v.0 as i64
    }
}

impl From<LiteralId> for u64 {
    fn from(v: LiteralId) -> Self {
        v.0 as u64
    }
}

/// ID associated to each pattern.
pub(crate) type PatternId = i32;

/// ID associated to each rule.
pub(crate) type RuleId = i32;

/// Structure that contains information and data structures required during the
/// current compilation process.
struct Context<'a, 'sym> {
    /// Builder for creating error and warning reports.
    report_builder: &'a ReportBuilder,

    /// Symbol table that contains the currently defined identifiers, modules,
    /// functions, etc.
    symbol_table: &'a mut StackedSymbolTable<'sym>,

    /// Symbol table for the currently active structure. When this contains
    /// some value, symbols are looked up in this table and the main symbol
    /// table (i.e: `symbol_table`) is ignored.
    current_struct: Option<Rc<dyn SymbolLookup + 'a>>,

    /// Used during code emitting for tracking the function signature
    /// associated to a function call.
    current_signature: Option<usize>,

    /// Table with all the symbols (functions, variables) used by WASM.
    wasm_symbols: WasmSymbols,

    /// Map where keys are fully qualified and mangled function names, and
    /// values are the function's ID in the WASM module.
    wasm_funcs: &'a FxHashMap<String, FunctionId>,

    /// Source code that is being compiled.
    src: &'a SourceCode<'a>,

    /// Rule that is being compiled.
    current_rule: &'a Rule,

    /// Warnings generated during the compilation.
    warnings: &'a mut Vec<Warning>,

    /// Pool with identifiers used in the rules.
    ident_pool: &'a mut StringPool<IdentId>,

    /// Pool with literal strings used in the rules.
    lit_pool: &'a mut BStringPool<LiteralId>,

    /// Stack of installed exception handlers for catching undefined values.
    exception_handler_stack: Vec<(ValType, InstrSeqId)>,

    /// Top of the variables stack. Starts at 0 and gets incremented by 1
    /// with each call to [`Context::new_var`].
    vars_stack_top: i32,

    lookup_start: Option<Var>,
    lookup_stack: VecDeque<i32>,
}

impl<'a, 'sym> Context<'a, 'sym> {
    /// Given an [`IdentId`] returns the identifier as `&str`.
    ///
    /// # Panics
    ///
    /// Panics if no identifier has the provided [`IdentId`].
    #[inline]
    fn resolve_ident(&self, ident_id: IdentId) -> &str {
        self.ident_pool.get(ident_id).unwrap()
    }

    /// Allocates space for a new variable in the stack of local variables.
    ///
    /// Do not confuse this stack with the WASM runtime stack (where WASM
    /// instructions take their operands from and put their results into).
    /// This is a completely unrelated stack used mainly for storing loop
    /// variables.
    ///
    /// This stack is stored in WASM main memory, in a memory region that goes
    /// from [`wasm::VARS_STACK_START`] to [`wasm::VARS_STACK_END`]. The stack
    /// is also mirrored at host-side (with host-side we refer to Rust code
    /// called from WASM code), because values like structures, maps, and
    /// arrays can't be handled by WASM code directly, and they must be
    /// accessible to Rust functions called from WASM. These two stacks (the
    /// WASM-side stack and the host-side stack) could be fully independent,
    /// but they are mirrored for simplicity. This means that calls to this
    /// function reserves space in both stacks at the same time, and therefore
    /// their sizes are always the same.
    ///
    /// However, each stack slot is used either by WASM-side code or by
    /// host-side code, but not by both. The slots that are used by WASM-side
    /// remain with empty values in the host-side stack, while the slots that
    /// are used by host-side code remain unused and undefined in WASM
    /// memory.
    ///
    /// # Panics
    ///
    /// Panics if the stack grows past [`wasm::VARS_STACK_END`]
    #[inline]
    fn new_var(&mut self, ty: Type) -> Var {
        let top = self.vars_stack_top;
        self.vars_stack_top += 1;
        if self.vars_stack_top * mem::size_of::<i64>() as i32
            > wasm::VARS_STACK_END - wasm::VARS_STACK_START
        {
            panic!("too many nested loops");
        }
        Var { ty, index: top }
    }

    /// Frees stack space previously allocated with [`Context::new_var`].
    ///
    /// This function restores the top of the stack to the value provided in
    /// the argument, effectively releasing all the stack space after that
    /// offset. For example:
    ///
    /// ```text
    /// let var1 = ctx.new_var()
    /// let var2 = ctx.new_var()
    /// let var3 = ctx.new_var()
    ///
    /// // Frees both var2 and var3, because var3 was allocated after var2
    /// ctx.free_vars(var2)
    /// ```
    #[inline]
    fn free_vars(&mut self, top: Var) {
        self.vars_stack_top = top.index;
    }

    /// Given a pattern identifier (e.g. `$a`) search for it in the current
    /// rule and return its [`PatternID`].
    ///
    /// # Panics
    ///
    /// Panics if the current rule does not have the requested pattern.
    fn get_pattern_from_current_rule(&self, ident: &Ident) -> PatternId {
        for (ident_id, pattern_id) in &self.current_rule.patterns {
            if self.resolve_ident(*ident_id) == ident.as_str() {
                return *pattern_id;
            }
        }
        panic!(
            "rule `{}` does not have pattern `{}` ",
            self.resolve_ident(self.current_rule.ident),
            ident.as_str()
        );
    }

    /// Given a function name returns its id.
    ///
    /// # Panics
    ///
    /// If a no function with the given name exists.
    pub fn function_id(&self, fn_name: &str) -> FunctionId {
        *self
            .wasm_funcs
            .get(fn_name)
            .unwrap_or_else(|| panic!("can't find function `{}`", fn_name))
    }
}

/// Represents a local variable returned by [`Context::new_var`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Var {
    ty: Type,
    index: i32,
}

/// A set of YARA rules in compiled form.
///
/// This is the result from [`Compiler::build`].
pub struct Rules {
    /// Pool with identifiers used in the rules. Each identifier has its
    /// own [`IdentId`], which can be used for retrieving the identifier
    /// from the pool as a `&str`.
    ident_pool: StringPool<IdentId>,

    /// Pool with literal strings used in the rules. Each literal has its
    /// own [`LiteralId`], which can be used for retrieving the literal
    /// string as `&BStr`.
    lit_pool: BStringPool<LiteralId>,

    /// WebAssembly module containing the code for all rule conditions.
    wasm_mod: Module,

    /// WebAssembly module already compiled into native code for the current
    /// platform.
    compiled_wasm_mod: wasmtime::Module,

    /// Vector with the names of all the imported modules. The vector contains
    /// the [`IdentId`] corresponding to the module's identifier.
    imported_modules: Vec<IdentId>,

    /// Vector containing all the compiled rules. A [`RuleId`] is an index
    /// in this vector.
    rules: Vec<Rule>,

    /// Vector with all the patterns used in the rules. This vector has not
    /// duplicated items, if two different rules use the "MZ" pattern, it
    /// appears in this list once. A [`PatternId`] is an index in this
    /// vector.
    patterns: Vec<Pattern>,
}

impl Rules {
    /// Returns an slice with the individual rules that were compiled.
    #[inline]
    pub fn rules(&self) -> &[Rule] {
        self.rules.as_slice()
    }

    /// Returns an slice with the individual patterns that were compiled.
    #[inline]
    pub fn patterns(&self) -> &[Pattern] {
        self.patterns.as_slice()
    }

    /// An iterator that yields the name of the modules imported by the
    /// rules.
    pub fn imports(&self) -> Imports {
        Imports {
            iter: self.imported_modules.iter(),
            ident_pool: &self.ident_pool,
        }
    }

    #[inline]
    pub(crate) fn lit_pool(&self) -> &BStringPool<LiteralId> {
        &self.lit_pool
    }

    #[inline]
    pub(crate) fn ident_pool(&self) -> &StringPool<IdentId> {
        &self.ident_pool
    }

    #[inline]
    pub(crate) fn compiled_wasm_mod(&self) -> &wasmtime::Module {
        &self.compiled_wasm_mod
    }
}

/// Iterator that yields the names of the modules imported by the rules.
pub struct Imports<'a> {
    iter: std::slice::Iter<'a, IdentId>,
    ident_pool: &'a StringPool<IdentId>,
}

impl<'a> Iterator for Imports<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|id| self.ident_pool.get(*id).unwrap())
    }
}

/// Each of the individual rules included in [`Rules`].
pub struct Rule {
    /// The ID of the rule identifier in the identifiers pool.
    pub(crate) ident: IdentId,

    /// Vector with all the patterns defined by this rule.
    patterns: Vec<(IdentId, PatternId)>,
}

/// A pattern (a.k.a string) in the compiled rules.
pub struct Pattern {}
