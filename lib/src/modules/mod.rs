use lazy_static::lazy_static;
use protobuf::reflect::MessageDescriptor;
use protobuf::MessageDyn;
use rustc_hash::FxHashMap;

pub mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
}

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub(crate) mod prelude {
    pub(crate) use crate::scanner::ScanContext;
    pub(crate) use crate::wasm::string::*;
    pub(crate) use crate::wasm::*;
    pub(crate) use bstr::ByteSlice;
    pub(crate) use linkme::distributed_slice;
    pub(crate) use wasmtime::Caller;
    pub(crate) use yara_x_macros::{module_export, module_main, wasm_export};
}

include!("modules.rs");

/// Type of module's main function.
type MainFn = fn(&[u8], Option<&[u8]>) -> Box<dyn MessageDyn>;

/// Describes a YARA module.
pub(crate) struct Module {
    /// Pointer to the module's main function.
    pub main_fn: Option<MainFn>,
    /// Name of the Rust module, if any, that contains code for this YARA
    /// module (e.g: "test_proto2").
    pub rust_module_name: Option<&'static str>,
    /// A [`MessageDescriptor`] that describes the module's structure. This
    /// corresponds to the protobuf message declared in the "root_message"
    /// for the YARA module. It allows iterating the fields declared by the
    /// module and obtaining their names and types.
    pub root_struct_descriptor: MessageDescriptor,
}

/// Macro that adds a module to the `BUILTIN_MODULES` map.
///
/// This macro is used by `add_modules.rs`, a file that is automatically
/// generated by `build.rs` based on the Protocol Buffers defined in the
/// `src/modules/protos` directory.
///
/// # Example
///
/// add_module!(modules, "test", test, "Test", test_mod, Some(test::main as
/// MainFn));
macro_rules! add_module {
    ($modules:expr, $name:literal, $proto:ident, $root_message:literal, $rust_module_name:expr, $main_fn:expr) => {{
        use std::stringify;
        let root_struct_descriptor = protos::$proto::file_descriptor()
            // message_by_full_name expects a dot (.) at the beginning
            // of the name.
            .message_by_full_name(format!(".{}", $root_message).as_str())
            .expect(format!(
                "`root_message` option in protobuf `{}` is wrong, message `{}` is not defined",
                stringify!($proto),
                $root_message
            ).as_str());

        $modules.insert(
            $name,
            Module {
                main_fn: $main_fn,
                rust_module_name: $rust_module_name,
                root_struct_descriptor,
            },
        );
    }};
}

lazy_static! {
    /// `BUILTIN_MODULES` is a static, global map where keys are module names
    /// and values are [`Module`] structures that describe a YARA module.
    ///
    /// This table is populated with the modules defined by a `.proto` file in
    /// `src/modules/protos`. Each `.proto` file that contains a statement like
    /// the following one defines a YARA module:
    ///
    /// ```protobuf
    /// option (yara.module_options) = {
    ///   name : "foo"
    ///   root_message: "Foo"
    ///   rust_module: "foo"
    /// };
    /// ```
    ///
    /// The `name` field is the module's name (i.e: the name used in `import`
    /// statements), which is also the key in `BUILTIN_MODULES`. `root_message`
    /// is the name of the message that describes the module's structure. This
    /// is required because a `.proto` file can define more than one message.
    ///
    /// `rust_module` is the name of the Rust module where functions exported
    /// by the YARA module are defined. This field is optional, if not provided
    /// the module is considered a data-only module.
    pub(crate) static ref BUILTIN_MODULES: FxHashMap<&'static str, Module> = {
        let mut modules = FxHashMap::default();
        // The `add_modules.rs` file is automatically generated at compile time
        // by `build.rs`. This is an example of how `add_modules.rs` looks like:
        //
        // {
        //  #[cfg(feature = "pe_module")]
        //  add_module!(modules, "pe", pe, Some(pe::main as MainFn));
        //
        //  #[cfg(feature = "elf_module")]
        //  add_module!(modules, "elf", elf, Some(elf::main as MainFn));
        // }
        //
        // `add_modules.rs` will contain an `add_module!` statement for each
        // protobuf in `src/modules/protos` defining a YARA module.
        include!("add_modules.rs");
        modules
    };
}

pub mod mods {
    /*! Utility functions and structures for invoking YARA modules directly.

    The utility functions [`invoke`], [`invoke_dyn`] and [`invoke_all`]
    allow leveraging YARA modules for parsing some file formats independently
    of any YARA rule. With these functions you can pass arbitrary data to a
    YARA module and obtain the same data structure that is accessible to YARA
    rules and which you use in your rule conditions.

    This allows external projects to benefit from YARA's file-parsing
    capabilities for their own purposes.

    # Example

    ```rust
    # use yara_x;
    let pe_info = yara_x::mods::invoke::<yara_x::mods::PE>(&[]);
    ```
     */

    /// Data structures defined by the `dotnet` module.
    ///
    /// The main structure produced by the module is [`dotnet::Dotnet`]. The
    /// rest of them are used by one or more fields in the main structure.
    ///
    pub use super::protos::dotnet;
    /// Data structure returned by the `dotnet` module.
    pub use super::protos::dotnet::Dotnet;

    /// Data structures defined by the `elf` module.
    ///
    /// The main structure produced by the module is [`elf::ELF`]. The rest of
    /// them are used by one or more fields in the main structure.
    ///
    pub use super::protos::elf;
    /// Data structure returned by the `elf` module.
    pub use super::protos::elf::ELF;

    /// Data structures defined by the `lnk` module.
    ///
    /// The main structure produced by the module is [`lnk::Lnk`]. The rest of
    /// them are used by one or more fields in the main structure.
    ///
    pub use super::protos::lnk;
    /// Data structure returned by the `lnk` module.
    pub use super::protos::lnk::Lnk;

    /// Data structures defined by the `macho` module.
    ///
    /// The main structure produced by the module is [`macho::Macho`]. The rest
    /// of them are used by one or more fields in the main structure.
    ///
    pub use super::protos::macho;
    /// Data structure returned by the `macho` module.
    pub use super::protos::macho::Macho;

    /// Data structures defined by the `pe` module.
    ///
    /// The main structure produced by the module is [`pe::PE`]. The rest
    /// of them are used by one or more fields in the main structure.
    ///
    pub use super::protos::pe;
    /// Data structure returned by the `pe` module.
    pub use super::protos::pe::PE;

    /// A data structure contains the data returned by all modules.
    pub use super::protos::mods::Modules;

    /// Invoke a YARA module with arbitrary data.
    ///
    /// <br>
    ///
    /// YARA modules typically parse specific file formats, returning structures
    /// that contain information about the file. These structures are used in YARA
    /// rules for expressing powerful and rich conditions. However, being able to
    /// access this information outside YARA rules can also be beneficial.
    ///
    /// <br>
    ///
    /// This function allows the direct invocation of a YARA module for parsing
    /// arbitrary data. It returns the structure produced by the module, which
    /// depends upon the invoked module. The result will be [`None`] if the
    /// module does not exist, or if it doesn't produce any information for
    /// the input data.
    ///
    /// `T` must be one of the structure types returned by a YARA module, which
    /// are defined [`crate::mods`], like [`crate::mods::PE`], [`crate::mods::ELF`], etc.
    ///
    /// # Example
    /// ```rust
    /// # use yara_x;
    /// let elf_info = yara_x::mods::invoke::<yara_x::mods::ELF>(&[]);
    /// ```
    pub fn invoke<T: protobuf::MessageFull>(data: &[u8]) -> Option<Box<T>> {
        let module_output = invoke_dyn::<T>(data)?;
        Some(<dyn protobuf::MessageDyn>::downcast_box(module_output).unwrap())
    }

    /// Like [`invoke`], but allows passing metadata to the module.
    pub fn invoke_with_meta<T: protobuf::MessageFull>(
        data: &[u8],
        meta: Option<&[u8]>,
    ) -> Option<Box<T>> {
        let module_output = invoke_with_meta_dyn::<T>(data, meta)?;
        Some(<dyn protobuf::MessageDyn>::downcast_box(module_output).unwrap())
    }

    /// Invoke a YARA module with arbitrary data, but returns a dynamic
    /// structure.
    ///
    /// This function is similar to [`invoke`] but its result is a dynamic-
    /// dispatch version of the structure returned by the YARA module.
    pub fn invoke_dyn<T: protobuf::MessageFull>(
        data: &[u8],
    ) -> Option<Box<dyn protobuf::MessageDyn>> {
        invoke_with_meta_dyn::<T>(data, None)
    }

    /// Like [`invoke_dyn`], but allows passing metadata to the module.
    pub fn invoke_with_meta_dyn<T: protobuf::MessageFull>(
        data: &[u8],
        meta: Option<&[u8]>,
    ) -> Option<Box<dyn protobuf::MessageDyn>> {
        let descriptor = T::descriptor();
        let proto_name = descriptor.full_name();
        let (_, module) =
            super::BUILTIN_MODULES.iter().find(|(_, module)| {
                module.root_struct_descriptor.full_name() == proto_name
            })?;

        Some(module.main_fn?(data, meta))
    }

    /// Invoke all YARA modules and return the data produced by them.
    ///
    /// This function is similar to [`invoke`], but it returns the
    /// information produced by all modules at once.
    ///
    /// # Example
    /// ```rust
    /// # use yara_x;
    /// let modules_output = yara_x::mods::invoke_all(&[]);
    /// ```
    pub fn invoke_all(data: &[u8]) -> Box<Modules> {
        let mut info = Box::new(Modules::new());
        info.pe = protobuf::MessageField(invoke::<PE>(data));
        info.elf = protobuf::MessageField(invoke::<ELF>(data));
        info.dotnet = protobuf::MessageField(invoke::<Dotnet>(data));
        info.macho = protobuf::MessageField(invoke::<Macho>(data));
        info.lnk = protobuf::MessageField(invoke::<Lnk>(data));
        info
    }

    /// Iterator over built-in module names.
    ///
    /// See the "debug modules" command.
    pub fn module_names() -> impl Iterator<Item = &'static str> {
        super::BUILTIN_MODULES.keys().copied()
    }
}
