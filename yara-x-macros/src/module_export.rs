use darling::FromMeta;
use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{AttributeArgs, Expr, FnArg, ItemFn, Pat};

#[derive(Debug, FromMeta)]
/// Arguments received by the `#[module_export]` macro.
pub struct ModuleExportsArgs {
    name: Option<String>,
}

/// Implementation for the `#[module_export]` attribute macro.
///
/// This attribute is used for exporting functions from YARA modules, to make
/// them callable from rules. The implementation is just a thin layer on top
/// of `#[wasm_export]` that simply adds an intermediate function converts the
/// argument `caller: Caller<'_, ScanContext>` into `ctx: &ScanContext`.
///
/// # Example
///
/// Suppose that our function is:
///
/// ```text
/// #[module_export]
/// fn add(ctx: &ScanContext, a: i64, b: i64) -> i64 {
///     a + b
/// }
/// ```
///
/// The code generated will be:
///
/// ```text
/// use wasmtime::Caller;
///
/// #[wasm_export(path = "add")]
/// fn __thunk__add(caller: Caller<'_, ScanContext>, a: i64, b: i64) -> i64 {
///   add(caller.data(), a, b)
/// }
///
/// fn add(ctx: &ScanContext, a: i64, b: i64) -> i64 {
///   a + b
/// }
/// ```
pub(crate) fn impl_module_export_macro(
    attr_args: AttributeArgs,
    mut func: ItemFn,
) -> syn::Result<TokenStream> {
    let attr_args = ModuleExportsArgs::from_list(&attr_args)?;

    // Include the original function in the output without changes.
    let mut token_stream = quote! {
        #func
    }
    .to_token_stream();

    // Create new arguments that are exactly the same arguments in the
    // original function, except the first one which changes from
    // &ScanContext to Caller<'_, ScanContext>.
    let mut fn_args: Punctuated<FnArg, Comma> = Punctuated::new();

    fn_args.push(syn::parse2(quote! {
        mut caller: Caller<'_, ScanContext>
    })?);

    fn_args.extend(func.sig.inputs.into_iter().skip(1));

    let mut arg_pats: Punctuated<Expr, Comma> = Punctuated::new();

    for arg in fn_args.iter().skip(1).cloned() {
        if let FnArg::Typed(pat_type) = arg {
            if let Pat::Ident(ident) = *pat_type.pat {
                arg_pats.push(Expr::Verbatim(quote! {#ident}));
            } else {
                unreachable!()
            }
        } else {
            unreachable!()
        }
    }

    let fn_name_str = if let Some(name) = attr_args.name {
        name
    } else {
        func.sig.ident.to_string()
    };

    let fn_name = func.sig.ident;

    // Modify the original function and convert it into the thunk function.
    func.sig.ident = format_ident!("__thunk__{}", fn_name);
    func.sig.inputs = fn_args;

    func.block = syn::parse2(quote! {{
        #fn_name(caller.data_mut(), #arg_pats)
    }})
    .unwrap();

    // Add the thunk function to the output.
    token_stream.extend(quote! {
        #[wasm_export(name = #fn_name_str, public = true)]
        #[inline(always)]
        #[allow(non_snake_case)]
        #func
    });

    Ok(token_stream)
}
