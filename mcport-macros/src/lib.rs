//! Compile-time adapters that turn ordinary Rust functions into MCPort tools.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, parse_quote, Attribute, Error, Expr, FnArg, Ident, ItemFn, LitStr, Meta,
    Pat, Path, Token, Type,
};

#[derive(Default)]
struct ToolAttr {
    name: Option<LitStr>,
    title: Option<LitStr>,
    read_only: bool,
    closed_world: bool,
    crate_path: Option<Path>,
}

impl ToolAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut attr = Self::default();
        if input.is_empty() {
            return Ok(attr);
        }
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        for meta in metas {
            match meta {
                Meta::Path(path) if path.is_ident("read_only") => attr.read_only = true,
                Meta::Path(path) if path.is_ident("closed_world") => attr.closed_world = true,
                Meta::NameValue(nv) if nv.path.is_ident("name") => {
                    attr.name = Some(lit_str_from_expr(&nv.value, "name")?);
                }
                Meta::NameValue(nv) if nv.path.is_ident("title") => {
                    attr.title = Some(lit_str_from_expr(&nv.value, "title")?);
                }
                Meta::NameValue(nv) if nv.path.is_ident("crate") => match &nv.value {
                    Expr::Path(path) => attr.crate_path = Some(path.path.clone()),
                    other => {
                        return Err(Error::new_spanned(
                            other,
                            "crate must be a path such as `mcport` or `::mcport`",
                        ));
                    }
                },
                other => {
                    return Err(Error::new_spanned(
                        other,
                        "unsupported #[tool] argument; expected name, title, crate, read_only, or closed_world",
                    ));
                }
            }
        }
        Ok(attr)
    }
}

#[derive(Default)]
struct ArgAttr {
    description: Option<LitStr>,
    default: Option<Expr>,
    min: Option<Expr>,
    max: Option<Expr>,
    state: bool,
    context: bool,
}

impl ArgAttr {
    fn apply(&mut self, attr: &Attribute) -> syn::Result<bool> {
        let path = attr.path();
        if path.is_ident("state") {
            self.state = true;
            return Ok(true);
        }
        if path.is_ident("context") {
            self.context = true;
            return Ok(true);
        }
        if !path.is_ident("arg") {
            return Ok(false);
        }
        if matches!(attr.meta, Meta::Path(_)) {
            return Ok(true);
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("description") {
                self.description = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            } else if meta.path.is_ident("default") {
                self.default = Some(meta.value()?.parse::<Expr>()?);
                Ok(())
            } else if meta.path.is_ident("min") {
                self.min = Some(meta.value()?.parse::<Expr>()?);
                Ok(())
            } else if meta.path.is_ident("max") {
                self.max = Some(meta.value()?.parse::<Expr>()?);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported #[arg] argument; expected description, default, min, or max",
                ))
            }
        })?;
        Ok(true)
    }
}

struct Argument {
    ident: Ident,
    ty: Type,
    attr: ArgAttr,
}

/// Marks an ordinary function as an MCP tool and generates its adapter.
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr with ToolAttr::parse);
    let input = parse_macro_input!(item as ItemFn);
    expand_tool(attr, input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// Collects `#[tool]` adapters for [`mcport::McpServer::tools`](mcport::McpServer::tools).
#[proc_macro]
pub fn tools(input: TokenStream) -> TokenStream {
    expand_tools(input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// Implements [`mcport::IntoToolReply`](mcport::IntoToolReply) for a serializable struct.
#[proc_macro_derive(ToolOutput)]
pub fn derive_tool_output(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        impl #impl_generics ::mcport::IntoToolReply for #name #ty_generics #where_clause {
            fn into_tool_reply(self) -> ::mcport::ToolReply {
                ::mcport::ToolReply::structured(self)
            }
        }
    }
    .into()
}

/// Implements [`mcport::ToolSchema`](mcport::ToolSchema) for a simple struct.
#[proc_macro_derive(ToolSchema, attributes(schema))]
pub fn derive_tool_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    expand_tool_schema(input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn expand_tool(attr: ToolAttr, mut input: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if input.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            input.sig.asyncness,
            "#[tool] functions must be synchronous; MCPort has no async runtime",
        ));
    }
    if !input.sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &input.sig.generics,
            "#[tool] does not support generic functions; use concrete argument types",
        ));
    }
    if let Some(receiver) = input.sig.inputs.iter().find_map(|arg| match arg {
        FnArg::Receiver(receiver) => Some(receiver),
        FnArg::Typed(_) => None,
    }) {
        return Err(Error::new_spanned(
            receiver,
            "#[tool] must be applied to a free function, not a method",
        ));
    }

    let crate_path = attr
        .crate_path
        .clone()
        .unwrap_or_else(|| parse_quote!(::mcport));
    let fn_ident = input.sig.ident.clone();
    let tool_name = attr
        .name
        .as_ref()
        .map(LitStr::value)
        .unwrap_or_else(|| fn_ident.to_string());
    let description = extract_docs(&input.attrs);
    let arguments = collect_arguments(&input)?;
    if arguments.iter().any(|argument| argument.attr.context) {
        return Err(Error::new_spanned(
            &input.sig,
            "#[context] is not part of the first #[tool] release; pass RequestContext through application state or a raw handler",
        ));
    }

    let state_args = arguments
        .iter()
        .filter(|argument| argument.attr.state)
        .collect::<Vec<_>>();
    if state_args.len() > 1 {
        return Err(Error::new_spanned(
            &input.sig,
            "#[tool] accepts at most one #[state] parameter",
        ));
    }
    let inputs = arguments
        .iter()
        .filter(|argument| !argument.attr.state)
        .collect::<Vec<_>>();

    let args_ident = format_ident!("__McportArgs_{fn_ident}");
    let register_ident = format_ident!("__mcport_register_{fn_ident}");
    let invoke_ident = format_ident!("__mcport_invoke_{fn_ident}");
    let vis = input.vis.clone();

    let field_defs = inputs.iter().map(|argument| {
        let ident = &argument.ident;
        let ty = &argument.ty;
        if argument.attr.default.is_some() {
            let default_fn = format_ident!("__mcport_default_{fn_ident}_{ident}");
            let default_fn_path = default_fn.to_string();
            quote! {
                #[serde(default = #default_fn_path)]
                #ident: #ty
            }
        } else {
            quote! { #ident: #ty }
        }
    });
    let default_fns = inputs.iter().filter_map(|argument| {
        let default = argument.attr.default.as_ref()?;
        let ident = &argument.ident;
        let ty = &argument.ty;
        let default_fn = format_ident!("__mcport_default_{fn_ident}_{ident}");
        Some(quote! {
            #[doc(hidden)]
            fn #default_fn() -> #ty {
                #default
            }
        })
    });

    let schema_inserts = inputs.iter().map(|argument| {
        let ident = argument.ident.to_string();
        let ty = &argument.ty;
        let required = argument.attr.default.is_none();
        let description = argument
            .attr
            .description
            .as_ref()
            .map_or_else(|| quote!(None), |text| quote!(Some(#text)));
        let default = argument.attr.default.as_ref().map_or_else(
            || quote!(None),
            |expr| quote!(#crate_path::__private::to_value(&(#expr)).ok()),
        );
        let min = argument
            .attr
            .min
            .as_ref()
            .map_or_else(|| quote!(None), |expr| quote!(Some((#expr) as f64)));
        let max = argument
            .attr
            .max
            .as_ref()
            .map_or_else(|| quote!(None), |expr| quote!(Some((#expr) as f64)));
        let required_push = required.then(|| {
            quote! {
                if <#ty as #crate_path::ToolSchema>::is_required() {
                    required.push(#ident.to_owned());
                }
            }
        });
        quote! {
            properties.insert(
                #ident.to_owned(),
                #crate_path::decorate_schema(
                    <#ty as #crate_path::ToolSchema>::tool_schema(),
                    #description,
                    #default,
                    #min,
                    #max,
                ),
            );
            #required_push
        }
    });

    let checks = inputs.iter().filter_map(|argument| {
        if argument.attr.min.is_none() && argument.attr.max.is_none() {
            return None;
        }
        let ident = &argument.ident;
        let name = ident.to_string();
        let min = argument.attr.min.as_ref();
        let max = argument.attr.max.as_ref();
        let condition = match (min, max) {
            (Some(min), Some(max)) => quote! { !(args.#ident >= #min && args.#ident <= #max) },
            (Some(min), None) => quote! { !(args.#ident >= #min) },
            (None, Some(max)) => quote! { !(args.#ident <= #max) },
            (None, None) => unreachable!(),
        };
        let message = match (min, max) {
            (Some(min), Some(max)) => {
                quote! { format!("{} must be between {} and {}", #name, #min, #max) }
            }
            (Some(min), None) => quote! { format!("{} must be at least {}", #name, #min) },
            (None, Some(max)) => quote! { format!("{} must be at most {}", #name, #max) },
            (None, None) => unreachable!(),
        };
        Some(quote! {
            if #condition {
                return #crate_path::ToolReply::error(#message);
            }
        })
    });

    let call_args = arguments.iter().map(|argument| {
        let ident = &argument.ident;
        if argument.attr.state {
            if is_mut_ref(&argument.ty) {
                quote! { state }
            } else {
                quote! { &*state }
            }
        } else {
            quote! { args.#ident }
        }
    });

    let title = attr
        .title
        .as_ref()
        .map_or_else(|| quote!(None), |title| quote!(Some(#title.to_owned())));
    let read_only = attr.read_only;
    let open_world = if attr.closed_world {
        quote!(Some(false))
    } else {
        quote!(None)
    };

    let (state_ty, prepared) = if let Some(state) = state_args.first() {
        let inner = owned_state_type(&state.ty)?;
        (
            quote! { #inner },
            quote! {
                #crate_path::PreparedTool::<#inner>::stateful(definition, #invoke_ident)
            },
        )
    } else {
        (
            quote! { () },
            quote! {
                #crate_path::PreparedTool::<()>::stateless(definition, #invoke_ident)
            },
        )
    };

    let invoke_state_arg = if state_args.is_empty() {
        quote! { arguments: &str }
    } else {
        quote! { state: &mut #state_ty, arguments: &str }
    };
    let invoke_call = if state_args.is_empty() {
        quote! { #invoke_ident(arguments) }
    } else {
        quote! { #invoke_ident(state, arguments) }
    };
    let _ = invoke_call;

    strip_helper_attrs(&mut input);

    Ok(quote! {
        #input

        #(#default_fns)*

        #[derive(::serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(non_camel_case_types)]
        struct #args_ident {
            #(#field_defs,)*
        }

        #[doc(hidden)]
        fn #invoke_ident(#invoke_state_arg) -> #crate_path::ToolReply {
            match #crate_path::__private::from_str::<#args_ident>(arguments) {
                ::core::result::Result::Ok(args) => {
                    #(#checks)*
                    #crate_path::IntoToolReply::into_tool_reply(#fn_ident(#(#call_args),*))
                }
                ::core::result::Result::Err(error) => #crate_path::ToolReply::error(::std::format!(
                    "invalid arguments for {}: {error}",
                    #tool_name
                )),
            }
        }

        #[doc(hidden)]
        #[allow(non_snake_case)]
        #vis fn #register_ident() -> #crate_path::PreparedTool<#state_ty> {
            let mut properties = #crate_path::Map::new();
            let mut required = ::std::vec::Vec::<::std::string::String>::new();
            #(#schema_inserts)*
            let definition = #crate_path::ToolDefinition {
                name: #tool_name.to_owned(),
                description: #description.to_owned(),
                title: #title,
                input_schema: #crate_path::object_schema(properties, required),
                output_schema: None,
                annotations: #crate_path::ToolAnnotations {
                    read_only_hint: if #read_only { Some(true) } else { None },
                    destructive_hint: None,
                    idempotent_hint: None,
                    open_world_hint: #open_world,
                },
            };
            #prepared
        }
    })
}

fn expand_tools(input: TokenStream) -> syn::Result<proc_macro2::TokenStream> {
    let paths = syn::parse::<PathList>(input)?;
    let registers = paths.0.iter().map(register_fn_path);
    Ok(quote! { [ #( #registers() ),* ] })
}

fn expand_tool_schema(input: syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let syn::Data::Struct(data) = &input.data else {
        return Err(Error::new_spanned(
            &input.ident,
            "ToolSchema can only be derived for structs",
        ));
    };
    let syn::Fields::Named(fields) = &data.fields else {
        return Err(Error::new_spanned(
            &input.ident,
            "ToolSchema requires named fields",
        ));
    };
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let inserts = fields.named.iter().map(|field| {
        let ident = field.ident.as_ref().expect("named field");
        let ident_str = ident.to_string();
        let ty = &field.ty;
        quote! {
            properties.insert(
                #ident_str.to_owned(),
                <#ty as ::mcport::ToolSchema>::tool_schema(),
            );
            if <#ty as ::mcport::ToolSchema>::is_required() {
                required.push(#ident_str.to_owned());
            }
        }
    });
    Ok(quote! {
        impl #impl_generics ::mcport::ToolSchema for #name #ty_generics #where_clause {
            fn tool_schema() -> ::mcport::Value {
                let mut properties = ::mcport::Map::new();
                let mut required = ::std::vec::Vec::<::std::string::String>::new();
                #(#inserts)*
                ::mcport::object_schema(properties, required)
            }
        }
    })
}

struct PathList(Punctuated<Path, Token![,]>);

impl Parse for PathList {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let paths = Punctuated::parse_terminated(input)?;
        if paths.is_empty() {
            return Err(input.error("tools! requires at least one function path"));
        }
        Ok(Self(paths))
    }
}

fn register_fn_path(path: &Path) -> Path {
    let mut path = path.clone();
    if let Some(last) = path.segments.last_mut() {
        last.ident = format_ident!("__mcport_register_{}", last.ident);
        last.arguments = syn::PathArguments::None;
    }
    path
}

fn collect_arguments(input: &ItemFn) -> syn::Result<Vec<Argument>> {
    let mut arguments = Vec::new();
    for arg in &input.sig.inputs {
        let FnArg::Typed(pat) = arg else {
            continue;
        };
        let Pat::Ident(ident) = pat.pat.as_ref() else {
            return Err(Error::new_spanned(
                &pat.pat,
                "#[tool] arguments must be simple named parameters",
            ));
        };
        let mut attr = ArgAttr::default();
        for attribute in &pat.attrs {
            if !attr.apply(attribute)? && is_helper_attr(attribute) {
                return Err(Error::new_spanned(
                    attribute,
                    "unrecognized tool argument attribute",
                ));
            }
        }
        if attr.state && attr.context {
            return Err(Error::new_spanned(
                ident,
                "a parameter cannot be both #[state] and #[context]",
            ));
        }
        arguments.push(Argument {
            ident: ident.ident.clone(),
            ty: (*pat.ty).clone(),
            attr,
        });
    }
    Ok(arguments)
}

fn strip_helper_attrs(input: &mut ItemFn) {
    for arg in &mut input.sig.inputs {
        if let FnArg::Typed(pat) = arg {
            pat.attrs.retain(|attr| !is_helper_attr(attr));
        }
    }
}

fn is_helper_attr(attr: &Attribute) -> bool {
    let ident = attr.path().segments.last().map(|segment| &segment.ident);
    matches!(
        ident.map(ToString::to_string).as_deref(),
        Some("arg" | "state" | "context")
    )
}

fn extract_docs(attrs: &[Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        let Meta::NameValue(meta) = &attr.meta else {
            continue;
        };
        if !meta.path.is_ident("doc") {
            continue;
        }
        if let Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(text),
            ..
        }) = &meta.value
        {
            let line = text.value();
            lines.push(line.trim_start_matches(' ').to_owned());
        }
    }
    let description = lines.join("\n");
    if description.is_empty() {
        String::new()
    } else {
        description
    }
}

fn lit_str_from_expr(expr: &Expr, name: &str) -> syn::Result<LitStr> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(text),
            ..
        }) => Ok(text.clone()),
        other => Err(Error::new_spanned(
            other,
            format!("{name} must be a string literal"),
        )),
    }
}

fn is_mut_ref(ty: &Type) -> bool {
    matches!(ty, Type::Reference(reference) if reference.mutability.is_some())
}

fn owned_state_type(ty: &Type) -> syn::Result<Type> {
    match ty {
        Type::Reference(reference) => Ok((*reference.elem).clone()),
        other => Err(Error::new_spanned(
            other,
            "#[state] must be a reference such as `&Index` or `&mut Index`",
        )),
    }
}
