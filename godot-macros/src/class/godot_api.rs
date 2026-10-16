/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::ParseResult;
use crate::class::{transform_inherent_impl, transform_trait_impl};
use crate::util::{KvParser, bail, venial_parse_meta};

fn parse_trait_impl_attr(meta: TokenStream) -> Result<bool, venial::Error> {
    let item = venial_parse_meta(&meta, format_ident!("godot_api"), &quote! { fn func(); })?;
    let mut attr = KvParser::parse_required(item.attributes(), "godot_api", &meta)?;
    let unsafe_threaded = attr.handle_alone("unsafe_threaded")?;
    attr.finish()?;
    Ok(unsafe_threaded)
}

fn parse_inherent_impl_attr(meta: TokenStream) -> Result<super::InherentImplAttr, venial::Error> {
    let item = venial_parse_meta(&meta, format_ident!("godot_api"), &quote! { fn func(); })?;
    let mut attr = KvParser::parse_required(item.attributes(), "godot_api", &meta)?;
    let secondary = attr.handle_alone("secondary")?;
    let no_typed_signals = attr.handle_alone("no_typed_signals")?;
    let no_typed_rpcs = attr.handle_alone("no_typed_rpcs")?;
    attr.finish()?;

    if no_typed_signals && secondary {
        return bail!(
            meta,
            "#[godot_api]: keys `secondary` and `no_typed_signals` are mutually exclusive; secondary blocks allow no signals anyway"
        )?;
    }
    if no_typed_rpcs && secondary {
        return bail!(
            meta,
            "#[godot_api]: keys `secondary` and `no_typed_rpcs` are mutually exclusive; secondary blocks don't allow rpcs anyway"
        )?;
    }

    Ok(super::InherentImplAttr {
        secondary,
        no_typed_signals,
        no_typed_rpcs,
    })
}

pub fn attribute_godot_api(
    meta: TokenStream,
    input_decl: venial::Item,
) -> ParseResult<TokenStream> {
    let decl = match input_decl {
        venial::Item::Impl(decl) => decl,
        _ => bail!(
            input_decl,
            "#[godot_api] can only be applied on impl blocks",
        )?,
    };

    if decl.impl_generic_params.is_some() {
        bail!(
            &decl,
            "#[godot_api] does not support lifetimes or generic parameters",
        )?;
    }

    let Some(self_path) = decl.self_ty.as_path() else {
        return bail!(decl, "invalid Self type for #[godot_api] impl");
    };

    if decl.trait_ty.is_some() {
        let unsafe_threaded = parse_trait_impl_attr(meta)?;
        transform_trait_impl(decl, unsafe_threaded)
    } else {
        match parse_inherent_impl_attr(meta) {
            Ok(meta) => transform_inherent_impl(meta, decl, self_path),
            Err(err) => Err(err),
        }
    }
}
