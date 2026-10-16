/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use proc_macro2::{Delimiter, Group, Ident, TokenStream, TokenTree};
use quote::{ToTokens, format_ident, quote};

use crate::class::data_models::func::validate_receiver_extract_gdself;
use crate::class::{BeforeKind, SignatureInfo, into_signature_info, make_virtual_callback};
use crate::util::{KvParser, bail, ident};
use crate::{ParseResult, util};

/// Codegen for `#[godot_api] impl ISomething for MyType`.
///
/// The `handle_*` methods and `Decls` fields are named after the user-facing `I*` virtual; see `godot_core::registry` for the naming rule
/// across all layers.
pub fn transform_trait_impl(
    mut original_impl: venial::Impl,
    unsafe_threaded: bool,
) -> ParseResult<TokenStream> {
    let (class_name, trait_path, trait_base_class) =
        util::validate_trait_impl_virtual(&original_impl, "godot_api")?;

    let prv = quote! { ::godot::private };

    let register_docs = crate::docs::make_interface_impl_docs_registration(
        &original_impl.body_items,
        &class_name,
        &prv,
    );

    let mut iface = InterfaceBuilder {
        class_name: &class_name,
        trait_path: &trait_path,
        decls: IDecls::default(),
    };
    let mut removed_methods_idx = Vec::new();
    let mut deprecation_warnings = TokenStream::new();
    for (index, item) in original_impl.body_items.iter_mut().enumerate() {
        let venial::ImplMember::AssocFunction(method) = item else {
            continue;
        };

        // Rewrite deprecated method names to their new equivalents (preserving span).
        // To remove deprecation support, delete this call and the function it references.
        deprecation_warnings.extend(maybe_rename_deprecated_virtual(method));

        let func_attr = parse_virtual_func_attr(&method.attributes)?;
        let is_gd_self = func_attr.gd_self;
        let method_threading = func_attr.threading;
        if (unsafe_threaded || method_threading.can_run_threaded()) && is_gd_self {
            return bail!(
                method,
                "threaded virtual callbacks do not support #[func(gd_self)]"
            );
        }

        // `#[func]` is consumed by this attribute macro. Leaving a method-level `#[func(unsafe_threaded)]` on the emitted Rust trait impl would
        // make rustc try to resolve it as an unrelated attribute after expansion.
        let mut attr_index = 0usize;
        method.attributes.retain(|_| {
            let retain = !func_attr.consumed_attribute_indices.contains(&attr_index);
            attr_index += 1;
            retain
        });
        // Methods with gd_self will be rewritten outside trait.
        if is_gd_self {
            removed_methods_idx.push(index);
        }

        // Transport #[cfg] attributes to the virtual method's FFI glue, to ensure it won't be
        // registered in Godot if conditionally removed from compilation.
        let cfg_attrs = util::extract_cfg_attrs(&method.attributes)
            .into_iter()
            .collect::<Vec<_>>();

        let method_name_str = method.name.to_string();
        match method_name_str.as_str() {
            "register_class" => {
                validate_not_method_threaded(&method_threading, method)?;
                validate_not_gd_self(is_gd_self, method)?;
                iface.handle_register_class(cfg_attrs);
            }
            "init" => {
                validate_not_method_threaded(&method_threading, method)?;
                validate_not_gd_self(is_gd_self, method)?;
                iface.handle_init(cfg_attrs);
            }
            "to_string" => {
                validate_not_method_threaded(&method_threading, method)?;
                iface.handle_to_string(cfg_attrs, is_gd_self);
            }
            "on_notification" => {
                validate_not_method_threaded(&method_threading, method)?;
                // POSTINIT notification can't be handled with the gd_self receiver
                // since object will not be yet constructed.
                validate_not_gd_self(is_gd_self, method)?;
                iface.handle_on_notification(cfg_attrs);
            }
            "on_get" => {
                validate_not_method_threaded(&method_threading, method)?;
                iface.handle_on_get(cfg_attrs, is_gd_self);
            }
            "on_set" => {
                validate_not_method_threaded(&method_threading, method)?;
                iface.handle_on_set(cfg_attrs, is_gd_self);
            }
            "on_validate_property" => {
                validate_not_method_threaded(&method_threading, method)?;
                iface.handle_on_validate_property(cfg_attrs, is_gd_self);
            }
            "on_get_property_list" => {
                validate_not_method_threaded(&method_threading, method)?;
                iface.handle_on_get_property_list(cfg_attrs, is_gd_self);
            }
            "on_property_get_revert" => {
                validate_not_method_threaded(&method_threading, method)?;
                iface.handle_on_property_get_revert(cfg_attrs, is_gd_self);
            }
            regular_virtual_fn => {
                // All the non-special engine ones: ready(), process(), etc.
                // Can modify original_impl, concretely the fn body for f64->f32 conversions.
                let changed_function = iface.handle_regular_virtual_fn(
                    method,
                    regular_virtual_fn,
                    cfg_attrs,
                    is_gd_self,
                    method_threading,
                )?;

                // If the function is modified (e.g. process() declared with f32), apply changes here.
                // Borrow-checker: we cannot reassign whole function due to shared borrow on `method.attributes`.
                // Thus, separately update signature + body when needed.
                if let Some((new_params, new_body)) = changed_function {
                    method.params = new_params;
                    method.body = Some(new_body);
                }
            }
        }
    }
    let mut decls = iface.decls;

    // If there is no ready() method explicitly overridden, we need to add one, to ensure that __before_ready() is called to
    // initialize the OnReady fields.
    if is_possibly_node_class(&trait_base_class)
        && !decls
            .overridden_virtuals
            .iter()
            .any(|v| v.rust_method_name == "_ready")
    {
        let match_arm = OverriddenVirtualFn {
            cfg_attrs: vec![],
            rust_method_name: "_ready".to_string(),
            // Can't use `virtuals::ready` here, as the base class might not be `Node` (see above why such a branch is still added).
            godot_name_hash_constant: quote! { ::godot::private::virtuals::Node::ready },
            signature_info: SignatureInfo::fn_ready(),
            before_kind: BeforeKind::OnlyBefore,
            interface_trait: None,
            threading: VirtualThreading::Ordinary,
        };

        decls.overridden_virtuals.push(match_arm);
    }

    let tool_check = util::make_virtual_tool_check();

    let modifications = decls.modifiers.drain(..).map(|(cfg_attrs, modifier)| {
        quote! {
            #(#cfg_attrs)*
            { item = item.#modifier::<#class_name>(); }
        }
    });

    let item_constructor = quote! {
        {
            let mut item = #prv::ITraitImpl::new::<#class_name>();
            #(#modifications)*
            item
        }
    };

    // See also __default_virtual_call() codegen.
    let (hash_param, match_expr);
    if cfg!(since_api = "4.4") {
        hash_param = quote! { hash: u32, };
        match_expr = quote! { (name, hash) };
    } else {
        hash_param = TokenStream::new();
        match_expr = quote! { name };
    };

    let virtual_match_arms = decls
        .overridden_virtuals
        .iter()
        .map(|v| v.make_match_arm(&class_name, &trait_base_class, unsafe_threaded));

    // Keep method-level contract checks under the same `#[cfg]` as their virtual. A disabled
    // method must not force an otherwise ordinary class to opt into worker-thread storage.
    // Interface-level opt-in remains unconditional because the attribute applies to the impl
    // itself, including every regular virtual which is enabled in that build.
    let make_threaded_contract_check =
        |cfg_attrs: &[&venial::Attribute], conditional_cfg: Option<&TokenStream>| {
            let conditional_attr = conditional_cfg.map(|condition| quote! { #[cfg(#condition)] });
            quote! {
                #(#cfg_attrs)*
                #conditional_attr
                const _: () = {
                    fn __godot_require_threaded_contract<T: ::godot::obj::ThreadedGodotClass>() {}

                    fn __godot_check_threaded_interface_contract() {
                        __godot_require_threaded_contract::<#class_name>();
                    }

                    assert!(
                        ::godot::sys::UNSAFE_THREADED_CALLBACKS_SUPPORTED,
                        "threaded virtual callbacks are incompatible with lazy-function-tables"
                    );

                    assert!(
                        <#class_name as ::godot::obj::GodotClass>::__UNSAFE_THREADED_OPT_IN,
                        "threaded virtual callbacks require an explicit #[class(unsafe_threaded)] opt-in"
                    );

                    match <#class_name as ::godot::obj::GodotClass>::__THREADING {
                        ::godot::obj::ClassThreading::Blocking => (),
                        ::godot::obj::ClassThreading::SingleThreaded => {
                            panic!("threaded virtual callbacks require #[class(unsafe_threaded)]")
                        }
                    }
                };
            }
        };

    let threaded_contract_check = if unsafe_threaded {
        make_threaded_contract_check(&[], None)
    } else {
        decls
            .overridden_virtuals
            .iter()
            .filter(|virtual_fn| virtual_fn.threading.can_run_threaded())
            .map(|virtual_fn| {
                make_threaded_contract_check(
                    &virtual_fn.cfg_attrs,
                    virtual_fn.threading.conditional_cfg(),
                )
            })
            .collect()
    };

    let mut result = quote! {
        // #original_impl and gd_self_impls are inserted below.
        #decls

        impl ::godot::private::You_forgot_the_attribute__godot_api for #class_name {}

        #threaded_contract_check

        impl ::godot::obj::cap::ImplementsGodotVirtual for #class_name {
            fn __virtual_call(name: &str, #hash_param) -> ::godot::sys::GDExtensionClassCallVirtual {
                //println!("virtual_call: {}.{}", std::any::type_name::<Self>(), name);
                use ::godot::obj::UserClass as _;
                use ::godot::private::virtuals::#trait_base_class as virtuals;
                #tool_check

                match #match_expr {
                    #( #virtual_match_arms )*
                    _ => None,
                }
            }
        }

        ::godot::sys::shard_add!(#prv::__GODOT_SHARD_REGISTRY; #prv::ClassShard::new::<#class_name>(
            #prv::ShardItem::ITraitImpl(#item_constructor)
        ));

        #register_docs
    };

    // #decls still holds a mutable borrow to `original_impl`, so we mutate && append it afterwards.

    let mut gd_self_decls = Vec::new();
    for index in removed_methods_idx.into_iter().rev() {
        let venial::ImplMember::AssocFunction(mut method) = original_impl.body_items.remove(index)
        else {
            unreachable!("We made sure that it is a function earlier.")
        };

        method.attributes.retain(util::is_cfg_or_cfg_attr);

        gd_self_decls.push(method);
    }

    let gd_self_decl = quote! {
        #[allow(clippy::wrong_self_convention)]
        impl #class_name {
            #( #gd_self_decls )*
        }
    };

    gd_self_decl.to_tokens(&mut result);
    original_impl.to_tokens(&mut result);
    deprecation_warnings.to_tokens(&mut result);

    Ok(result)
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Handlers for individual symbols in #[godot_api].

/// Builds `I*` trait interfaces. Bundles parameters shared by all `handle_*` methods.
///
/// Per-method state like `cfg_attrs` and `is_gd_self` is passed as parameters, not stored here.
struct InterfaceBuilder<'a> {
    /// Name of the user's class (e.g. `Player`).
    class_name: &'a Ident,

    /// Trait being implemented (e.g. `INode2D`).
    trait_path: &'a venial::TypeExpr,

    /// Accumulated output, built up across all `handle_*` calls and consumed at the end.
    decls: IDecls<'a>,
}

impl<'a> InterfaceBuilder<'a> {
    fn handle_register_class(&mut self, cfg_attrs: Vec<&'a venial::Attribute>) {
        // Implements the trait once for each implementation of this method, forwarding the cfg attrs of each
        // implementation to the generated trait impl. If the cfg attrs allow for multiple implementations of
        // this method to exist, then Rust will generate an error, so we don't have to worry about the multiple
        // trait implementations actually generating an error, since that can only happen if multiple
        // implementations of the same method are kept by #[cfg] (due to user error).
        // Thus, by implementing the trait once for each possible implementation of this method (depending on
        // what #[cfg] allows), forwarding the cfg attrs, we ensure this trait impl will remain in the code if
        // at least one of the method impls are kept.
        let class_name = self.class_name;
        let trait_path = self.trait_path;
        let prev = &self.decls.register_class_impl;
        self.decls.register_class_impl = quote! {
            #prev

            #(#cfg_attrs)*
            impl ::godot::obj::cap::GodotRegisterClass for #class_name {
                fn __godot_register_class(builder: &mut ::godot::builder::GodotBuilder<Self>) {
                    <Self as #trait_path>::register_class(builder)
                }
            }
        };

        self.decls.add_modifier(cfg_attrs, "register");
    }

    fn handle_init(&mut self, cfg_attrs: Vec<&'a venial::Attribute>) {
        // If #[class(init)] or #[class(no_init)] is provided, deny overriding manual init().
        let class_name = self.class_name;
        let trait_path = self.trait_path;
        let deny_manual_init_macro = util::format_class_deny_manual_init_macro(class_name);

        let prev = &self.decls.init_impl;
        self.decls.init_impl = quote! {
            #prev
            #deny_manual_init_macro!();

            #(#cfg_attrs)*
            impl ::godot::obj::cap::GodotDefault for #class_name {
                fn __godot_user_init(base: ::godot::obj::Base<Self::Base>) -> Self {
                    <Self as #trait_path>::init(base)
                }
            }
        };

        self.decls.add_modifier(cfg_attrs, "create");
    }

    fn handle_to_string(&mut self, cfg_attrs: Vec<&'a venial::Attribute>, is_gd_self: bool) {
        self.decls.to_string_impl = self.make_virtual_impl(
            &cfg_attrs,
            is_gd_self,
            false,
            "GodotToString",
            |iface, recv| {
                quote! {
                    fn __godot_to_string(
                        mut this: ::godot::private::VirtualMethodReceiver<Self>,
                    ) -> ::godot::builtin::GString {
                        #iface::to_string(#recv)
                    }
                }
            },
        );
        self.decls.add_modifier(cfg_attrs, "to_string");
    }

    fn handle_on_notification(&mut self, cfg_attrs: Vec<&'a venial::Attribute>) {
        let class_name = self.class_name;
        let trait_path = self.trait_path;
        let inactive_check = make_inactive_class_check(TokenStream::new());
        let prev = &self.decls.on_notification_impl;
        self.decls.on_notification_impl = quote! {
            #prev

            #(#cfg_attrs)*
            impl ::godot::obj::cap::GodotNotification for #class_name {
                fn __godot_on_notification(&mut self, what: i32) {
                    #inactive_check
                    <Self as #trait_path>::on_notification(self, what.into())
                }
            }
        };

        self.decls.add_modifier(cfg_attrs, "on_notification");
    }

    fn handle_on_get(&mut self, cfg_attrs: Vec<&'a venial::Attribute>, is_gd_self: bool) {
        let inactive_check = make_inactive_class_check(quote! { None });
        self.decls.on_get_impl =
            self.make_virtual_impl(&cfg_attrs, is_gd_self, false, "GodotGet", |iface, recv| {
                quote! {
                    fn __godot_on_get(
                        mut this: ::godot::private::VirtualMethodReceiver<Self>,
                        property: ::godot::builtin::StringName,
                    ) -> Option<::godot::builtin::Variant> {
                        #inactive_check
                        #iface::on_get(#recv, property)
                    }
                }
            });
        self.decls.add_modifier(cfg_attrs, "on_get");
    }

    fn handle_on_set(&mut self, cfg_attrs: Vec<&'a venial::Attribute>, is_gd_self: bool) {
        let inactive_check = make_inactive_class_check(quote! { false });
        self.decls.on_set_impl =
            self.make_virtual_impl(&cfg_attrs, is_gd_self, true, "GodotSet", |iface, recv| {
                quote! {
                    fn __godot_on_set(
                        mut this: ::godot::private::VirtualMethodReceiver<Self>,
                        property: ::godot::builtin::StringName,
                        value: ::godot::builtin::Variant,
                    ) -> bool {
                        #inactive_check
                        #iface::on_set(#recv, property, value)
                    }
                }
            });
        self.decls.add_modifier(cfg_attrs, "on_set");
    }

    fn handle_on_validate_property(
        &mut self,
        cfg_attrs: Vec<&'a venial::Attribute>,
        is_gd_self: bool,
    ) {
        let inactive_check = make_inactive_class_check(TokenStream::new());
        self.decls.on_validate_property_impl = self.make_virtual_impl(
            &cfg_attrs,
            is_gd_self,
            false,
            "GodotValidateProperty",
            |iface, recv| {
                quote! {
                    fn __godot_on_validate_property(
                        mut this: ::godot::private::VirtualMethodReceiver<Self>,
                        property: &mut ::godot::register::info::PropertyInfo,
                    ) {
                        #inactive_check
                        #iface::on_validate_property(#recv, property);
                    }
                }
            },
        );
        self.decls.add_modifier(cfg_attrs, "on_validate_property");
    }

    #[cfg(before_api = "4.3")]
    fn handle_on_get_property_list(
        &mut self,
        cfg_attrs: Vec<&'a venial::Attribute>,
        _is_gd_self: bool,
    ) {
        self.decls.on_get_property_list_impl = quote! {
            #(#cfg_attrs)*
            compile_error!("`on_get_property_list` is only supported for Godot versions of at least 4.3");
        };
    }

    #[cfg(since_api = "4.3")]
    fn handle_on_get_property_list(
        &mut self,
        cfg_attrs: Vec<&'a venial::Attribute>,
        is_gd_self: bool,
    ) {
        self.decls.on_get_property_list_impl = self.make_virtual_impl(
            &cfg_attrs,
            is_gd_self,
            true,
            "GodotGetPropertyList",
            |iface, recv| {
                quote! {
                    fn __godot_on_get_property_list(
                        mut this: ::godot::private::VirtualMethodReceiver<Self>,
                    ) -> Vec<::godot::register::info::PropertyInfo> {
                        #iface::on_get_property_list(#recv)
                    }
                }
            },
        );
        self.decls.add_modifier(cfg_attrs, "on_get_property_list");
    }

    fn handle_on_property_get_revert(
        &mut self,
        cfg_attrs: Vec<&'a venial::Attribute>,
        is_gd_self: bool,
    ) {
        let inactive_check = make_inactive_class_check(quote! { None });
        self.decls.on_property_get_revert_impl = self.make_virtual_impl(
            &cfg_attrs,
            is_gd_self,
            false,
            "GodotPropertyGetRevert",
            |iface, recv| {
                quote! {
                    fn __godot_on_property_get_revert(
                        this: ::godot::private::VirtualMethodReceiver<Self>,
                        property: StringName,
                    ) -> Option<::godot::builtin::Variant> {
                        #inactive_check
                        #iface::on_property_get_revert(#recv, property)
                    }
                }
            },
        );
        self.decls.add_modifier(cfg_attrs, "on_property_get_revert");
    }

    fn handle_regular_virtual_fn(
        &mut self,
        original_method: &venial::Function,
        method_name: &str,
        cfg_attrs: Vec<&'a venial::Attribute>,
        has_gd_self: bool,
        threading: VirtualThreading,
    ) -> ParseResult<Option<(venial::Punctuated<venial::FnParam>, Group)>> {
        // Fresh ident for generated code (`virtuals::method_name` constant lookup).
        // Using original span would cause IDE to show wrong semantic color for the original function definition.
        let method_name_ident = ident(method_name);
        let mut method = util::reduce_to_signature(original_method);
        validate_receiver_extract_gdself(&mut method, has_gd_self, &original_method.name)?;

        // Godot-facing name begins with underscore.
        //
        // godot-codegen special-cases the virtual method called _init (which exists on a handful of classes, distinct from the default
        // constructor) to init_ext, to avoid Rust-side ambiguity. See godot_codegen::class_generator::virtual_method_name.
        let virtual_method_name = if method_name == "init_ext" {
            String::from("_init")
        } else {
            format!("_{method_name}")
        };

        let signature_info = into_signature_info(method, self.class_name, has_gd_self);

        let mut updated_function = None;
        // If there was a signature change (e.g. f32 -> f64 in process/physics_process), apply to new function tokens.
        if !signature_info.modified_param_types.is_empty() {
            let mut param_name = None;

            let mut new_params = original_method.params.clone();
            let mut original_ty_span = None;

            for (index, new_ty) in signature_info.modified_param_types.iter() {
                let venial::FnParam::Typed(typed) = &mut new_params.inner[*index].0 else {
                    panic!("unexpected parameter type: {new_params:?}");
                };

                // Capture original type span before replacing (e.g. the user's `f32`).
                original_ty_span = Some(typed.ty.span());

                typed.ty = new_ty.clone();
                param_name = Some(typed.name.clone());
            }

            let original_body = &original_method.body;
            let param_name = param_name.expect("parameter had no name");
            let original_ty_span = original_ty_span.expect("type had no span");

            // Currently hardcoded to f32/f64 exchange; can be generalized if needed.
            // Create f32 ident with the original type's span for proper syntax highlighting. Works here because f64 uses same semantic color.
            let f32_ty = Ident::new("f32", original_ty_span);

            let body_code = quote! {
                let #param_name = #param_name as #f32_ty;
                #original_body
            };

            // Set span from original body, or fallback to method name span.
            let span = match original_body {
                Some(body) => body.span(),
                None => original_method.name.span(),
            };

            let mut wrapping_body = Group::new(Delimiter::Brace, body_code);
            wrapping_body.set_span(span);

            updated_function = Some((new_params, wrapping_body));
        }

        // Overridden ready() methods additionally have an additional `__before_ready()` call (for OnReady inits).
        let before_kind = if method_name == "ready" {
            BeforeKind::WithBefore
        } else {
            BeforeKind::Without
        };

        // Note that, if the same method is implemented multiple times (with different cfg attr combinations),
        // then there will be multiple match arms annotated with the same cfg attr combinations, thus they will
        // be reduced to just one arm (at most, if the implementations aren't all removed from compilation) for
        // each distinct method.
        self.decls.overridden_virtuals.push(OverriddenVirtualFn {
            cfg_attrs,
            rust_method_name: virtual_method_name,
            // If ever the `I*` verbatim validation is relaxed (it won't work with use-renames or other weird edge cases), the approach
            // with godot::private::virtuals module could be changed to something like the following (GodotBase = nearest Godot base class):
            // __get_virtual_hash::<Self::GodotBase>("method")
            godot_name_hash_constant: quote! { virtuals::#method_name_ident },
            signature_info,
            before_kind,
            interface_trait: Some(self.trait_path.clone()),
            threading,
        });

        Ok(updated_function)
    }

    /// Generates a `impl ::godot::obj::cap::{cap_trait_name} for {class_name}` block with the correct receiver type.
    ///
    /// `make_fn` receives `(iface, recv)` tokens and should return the full `fn` item (signature + body).
    /// `iface` is the qualified dispatch path, e.g. `<Self as INode2D>` (or `Self` for gd_self).
    /// `recv` is the receiver expression to pass as first argument to the interface method.
    fn make_virtual_impl(
        &self,
        cfg_attrs: &[&venial::Attribute],
        is_gd_self: bool,
        is_mut: bool,
        cap_trait_name: &str,
        make_fn: impl FnOnce(&TokenStream, &TokenStream) -> TokenStream,
    ) -> TokenStream {
        let class_name = self.class_name;
        let trait_path = self.trait_path;

        let (receiver_path, iface, recv) = match (is_gd_self, is_mut) {
            (false, true) => (
                quote! { ::godot::private::RecvMut },
                quote! { <Self as #trait_path> },
                quote! { &mut *this.recv_self_mut() },
            ),
            (false, false) => (
                quote! { ::godot::private::RecvRef },
                quote! { <Self as #trait_path> },
                quote! { &*this.recv_self() },
            ),
            (true, _) => (
                quote! { ::godot::private::RecvGdSelf },
                quote! { Self },
                quote! { this.recv_gd() },
            ),
        };

        let fn_body = make_fn(&iface, &recv);
        let cap_trait = ident(cap_trait_name);

        quote! {
            #(#cfg_attrs)*
            impl ::godot::obj::cap::#cap_trait for #class_name {
                type Recv = #receiver_path;
                #fn_body
            }
        }
    }
} // impl IfaceBuilder

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Rest of implementation

/// Returns `false` if the given class does definitely not inherit `Node`, `true` otherwise.
///
/// `#[godot_api]` has currently no way of checking base class at macro-resolve time, so the `_ready` branch is unconditionally
/// added, even for classes that don't inherit from `Node`. As a best-effort, we exclude some very common non-Node classes explicitly, to
/// generate less useless code.
pub(crate) fn is_possibly_node_class(trait_base_class: &Ident) -> bool {
    !matches!(
        trait_base_class.to_string().as_str(), //.
        "Object"
            | "MainLoop"
            | "RefCounted"
            | "Resource"
            | "ResourceLoader"
            | "ResourceSaver"
            | "SceneTree"
            | "Script"
            | "ScriptExtension"
    )
}

#[cfg(before_api = "4.3")]
fn make_inactive_class_check(return_value: TokenStream) -> TokenStream {
    quote! {
        use ::godot::obj::UserClass as _;
        if ::godot::private::is_class_inactive(Self::__config().is_tool) {
            return #return_value;
        }
    }
}

#[cfg(since_api = "4.3")]
fn make_inactive_class_check(_return_value: TokenStream) -> TokenStream {
    TokenStream::new()
}

#[derive(Clone, Default)]
struct VirtualFuncAttr {
    gd_self: bool,
    threading: VirtualThreading,
    consumed_attribute_indices: Vec<usize>,
}

fn parse_virtual_func_attr(attributes: &[venial::Attribute]) -> ParseResult<VirtualFuncAttr> {
    let mut result = VirtualFuncAttr::default();

    let has_direct_func = if let Some(mut parser) = KvParser::parse(attributes, "func")? {
        result.gd_self = parser.handle_alone("gd_self")?;
        result.threading = if parser.handle_alone("unsafe_threaded")? {
            VirtualThreading::Always
        } else {
            VirtualThreading::Ordinary
        };
        parser.finish()?;
        true
    } else {
        false
    };

    for (index, attr) in attributes.iter().enumerate() {
        if attr
            .get_single_path_segment()
            .is_some_and(|name| name == "func")
        {
            result.consumed_attribute_indices.push(index);
            continue;
        }

        let Some(condition) = parse_cfg_attr_threaded_func(attr)? else {
            continue;
        };

        if has_direct_func || !result.consumed_attribute_indices.is_empty() {
            return bail!(
                attr,
                "only one direct or conditional #[func(...)] attribute is allowed"
            );
        }

        result.threading = VirtualThreading::Conditional(condition);
        result.consumed_attribute_indices.push(index);
    }

    Ok(result)
}

/// Parses the deliberately narrow form
/// `#[cfg_attr(condition, func(unsafe_threaded))]`.
///
/// This lets a library retain an ordinary virtual on single-threaded targets while selecting
/// the worker-thread contract for the same virtual elsewhere. Other `cfg_attr` attributes are
/// left untouched for rustc.
fn parse_cfg_attr_threaded_func(attr: &venial::Attribute) -> ParseResult<Option<TokenStream>> {
    if attr
        .get_single_path_segment()
        .is_none_or(|name| name != "cfg_attr")
    {
        return Ok(None);
    }

    let venial::AttributeValue::Group(_, tokens) = &attr.value else {
        return Ok(None);
    };

    let Some(comma_index) = tokens
        .iter()
        .position(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ','))
    else {
        return Ok(None);
    };

    let nested = &tokens[comma_index + 1..];
    let contains_func = nested
        .iter()
        .any(|token| matches!(token, TokenTree::Ident(ident) if ident == "func"));
    if !contains_func {
        return Ok(None);
    }

    let [TokenTree::Ident(func), TokenTree::Group(options)] = nested else {
        return bail!(
            attr,
            "conditional #[func] must use exactly #[cfg_attr(condition, func(unsafe_threaded))]"
        );
    };
    if func != "func" || options.delimiter() != Delimiter::Parenthesis {
        return bail!(
            attr,
            "conditional #[func] must use exactly #[cfg_attr(condition, func(unsafe_threaded))]"
        );
    }

    let option_tokens = options.stream().into_iter().collect::<Vec<_>>();
    let is_unsafe_threaded_only = matches!(
        option_tokens.as_slice(),
        [TokenTree::Ident(option)] if option == "unsafe_threaded"
    );
    if !is_unsafe_threaded_only {
        return bail!(
            attr,
            "conditional #[func] currently supports only func(unsafe_threaded)"
        );
    }

    let condition = tokens[..comma_index]
        .iter()
        .cloned()
        .collect::<TokenStream>();
    if condition.is_empty() {
        return bail!(attr, "conditional #[func] requires a cfg condition");
    }

    Ok(Some(condition))
}

fn validate_not_method_threaded(
    threading: &VirtualThreading,
    method: &venial::Function,
) -> ParseResult<()> {
    if threading.can_run_threaded() {
        bail!(
            &method,
            "#[func(unsafe_threaded)] can only be used on regular engine virtual methods"
        )
    } else {
        Ok(())
    }
}

fn validate_not_gd_self(is_gd_self: bool, method: &venial::Function) -> ParseResult<()> {
    if is_gd_self {
        bail!(
            &method,
            "Method {} can't be used with #[func(gd_self)].",
            method.name
        )
    } else {
        Ok(())
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

struct OverriddenVirtualFn<'a> {
    cfg_attrs: Vec<&'a venial::Attribute>,
    rust_method_name: String,
    /// Path to a pre-defined constant storing a `("_virtual_func", 123456789)` tuple with name and hash of the virtual function.
    ///
    /// Before Godot 4.4, this just stores the name `"_virtual_func"`.
    godot_name_hash_constant: TokenStream,
    signature_info: SignatureInfo,
    before_kind: BeforeKind,
    interface_trait: Option<venial::TypeExpr>,
    /// Whether only this virtual, rather than the complete interface implementation, accepts worker-thread dispatch.
    threading: VirtualThreading,
}

impl OverriddenVirtualFn<'_> {
    fn make_match_arm(
        &self,
        class_name: &Ident,
        trait_base_class: &Ident,
        unsafe_threaded: bool,
    ) -> TokenStream {
        let godot_name_hash_constant = &self.godot_name_hash_constant;

        // Lazily generate code for the actual work (calling user function).
        if unsafe_threaded {
            let cfg_attrs = self.cfg_attrs.iter();
            let method_callback = make_virtual_callback(
                class_name,
                trait_base_class,
                &self.signature_info,
                self.before_kind,
                self.interface_trait.as_ref(),
                true,
            );
            return quote! {
                #(#cfg_attrs)*
                #godot_name_hash_constant => #method_callback,
            };
        }

        match &self.threading {
            VirtualThreading::Ordinary => {
                let cfg_attrs = self.cfg_attrs.iter();
                let method_callback = make_virtual_callback(
                    class_name,
                    trait_base_class,
                    &self.signature_info,
                    self.before_kind,
                    self.interface_trait.as_ref(),
                    false,
                );
                quote! {
                    #(#cfg_attrs)*
                    #godot_name_hash_constant => #method_callback,
                }
            }
            VirtualThreading::Always => {
                let cfg_attrs = self.cfg_attrs.iter();
                let method_callback = make_virtual_callback(
                    class_name,
                    trait_base_class,
                    &self.signature_info,
                    self.before_kind,
                    self.interface_trait.as_ref(),
                    true,
                );
                quote! {
                    #(#cfg_attrs)*
                    #godot_name_hash_constant => #method_callback,
                }
            }
            VirtualThreading::Conditional(condition) => {
                let threaded_cfg_attrs = self.cfg_attrs.iter();
                let ordinary_cfg_attrs = self.cfg_attrs.iter();
                let threaded_callback = make_virtual_callback(
                    class_name,
                    trait_base_class,
                    &self.signature_info,
                    self.before_kind,
                    self.interface_trait.as_ref(),
                    true,
                );
                let ordinary_callback = make_virtual_callback(
                    class_name,
                    trait_base_class,
                    &self.signature_info,
                    self.before_kind,
                    self.interface_trait.as_ref(),
                    false,
                );
                quote! {
                    #(#threaded_cfg_attrs)*
                    #[cfg(#condition)]
                    #godot_name_hash_constant => #threaded_callback,
                    #(#ordinary_cfg_attrs)*
                    #[cfg(not(#condition))]
                    #godot_name_hash_constant => #ordinary_callback,
                }
            }
        }
    }
}

#[derive(Clone, Default)]
enum VirtualThreading {
    #[default]
    Ordinary,
    Always,
    Conditional(TokenStream),
}

impl VirtualThreading {
    fn can_run_threaded(&self) -> bool {
        !matches!(self, Self::Ordinary)
    }

    fn conditional_cfg(&self) -> Option<&TokenStream> {
        match self {
            Self::Conditional(condition) => Some(condition),
            Self::Ordinary | Self::Always => None,
        }
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

/// Accumulates various symbols defined inside a `#[godot_api]` macro.
#[derive(Default)]
struct IDecls<'a> {
    init_impl: TokenStream,
    to_string_impl: TokenStream,
    register_class_impl: TokenStream,
    on_notification_impl: TokenStream,
    on_get_impl: TokenStream,
    on_set_impl: TokenStream,
    on_get_property_list_impl: TokenStream,
    on_property_get_revert_impl: TokenStream,
    on_validate_property_impl: TokenStream,

    modifiers: Vec<(Vec<&'a venial::Attribute>, Ident)>,
    overridden_virtuals: Vec<OverriddenVirtualFn<'a>>,
}

/// If `method` uses a deprecated virtual name, rename it in-place (preserving span)
/// and return a deprecation warning token stream.
//
// To remove deprecation support, delete this function, its call site, and the
// corresponding marker functions in godot-core/src/deprecated.rs.
fn maybe_rename_deprecated_virtual(method: &mut venial::Function) -> TokenStream {
    let (new_name, deprecation_fn) = match method.name.to_string().as_str() {
        "get_property" => ("on_get", "virtual_method_get_property"),
        "set_property" => ("on_set", "virtual_method_set_property"),
        "validate_property" => ("on_validate_property", "virtual_method_validate_property"),
        "get_property_list" => ("on_get_property_list", "virtual_method_get_property_list"),
        "property_get_revert" => (
            "on_property_get_revert",
            "virtual_method_property_get_revert",
        ),
        _ => return TokenStream::new(),
    };

    let span = method.name.span();
    method.name = Ident::new(new_name, span);

    let mut deprecation_fn = ident(deprecation_fn);
    deprecation_fn.set_span(span);
    quote! {
        ::godot::__deprecated::emit_deprecated_warning!(#deprecation_fn);
    }
}

impl<'a> IDecls<'a> {
    fn add_modifier(&mut self, cfg_attrs: Vec<&'a venial::Attribute>, modifier: &str) {
        self.modifiers
            .push((cfg_attrs, format_ident!("with_{modifier}")));
    }
}

impl ToTokens for IDecls<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        self.init_impl.to_tokens(tokens);
        self.to_string_impl.to_tokens(tokens);
        self.on_notification_impl.to_tokens(tokens);
        self.register_class_impl.to_tokens(tokens);
        self.on_get_impl.to_tokens(tokens);
        self.on_set_impl.to_tokens(tokens);
        self.on_get_property_list_impl.to_tokens(tokens);
        self.on_property_get_revert_impl.to_tokens(tokens);
        self.on_validate_property_impl.to_tokens(tokens);
    }
}
