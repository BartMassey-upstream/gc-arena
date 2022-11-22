use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use syn::spanned::Spanned;
use synstructure::{decl_derive, AddBounds};

fn collect_derive(mut s: synstructure::Structure) -> TokenStream {
    // Deriving `Collect` must be done with care, because an implementation of `Drop` is not
    // necessarily safe for `Collect` types.  This derive macro has three available modes to ensure
    // that this is safe:
    //   1) Require that the type be 'static with `#[collect(require_static)]`.
    //   2) Prohibit a `Drop` impl on the type with `#[collect(no_drop)]`
    //   3) Allow a custom `Drop` impl that might be unsafe with `#[collect(unsafe_drop)]`.  Such
    //      `Drop` impls must *not* access garbage collected pointers during `Drop::drop`.
    #[derive(PartialEq)]
    enum Mode {
        RequireStatic,
        NoDrop,
        UnsafeDrop,
    }

    let mut mode = None;
    let mut override_bound = None;

    for attr in &s.ast().attrs {
        match attr.parse_meta() {
            Ok(syn::Meta::List(syn::MetaList { path, nested, .. })) => {
                if path.is_ident("collect") {
                    if let Some(prev_mode) = mode {
                        let prev_mode_str = match prev_mode {
                            Mode::RequireStatic => "require_static",
                            Mode::NoDrop => "no_drop",
                            Mode::UnsafeDrop => "unsafe_drop",
                        };
                        panic!("`Collect` mode was already specified with `#[collect({})]`, cannot specify twice", prev_mode_str);
                    }

                    for nested in nested {
                        match nested {
                            syn::NestedMeta::Meta(syn::Meta::Path(path)) => {
                                if path.is_ident("require_static") {
                                    mode = Some(Mode::RequireStatic);
                                } else if path.is_ident("no_drop") {
                                    mode = Some(Mode::NoDrop);
                                } else if path.is_ident("unsafe_drop") {
                                    mode = Some(Mode::UnsafeDrop);
                                } else {
                                    panic!("`#[collect]` requires one of: \"require_static\", \"no_drop\", or \"unsafe_drop\" as an argument");
                                }
                            }
                            syn::NestedMeta::Meta(syn::Meta::NameValue(value)) => {
                                if value.path.is_ident("bound") {
                                    match value.lit {
                                        syn::Lit::Str(x) => override_bound = Some(x),
                                        _ => {
                                            panic!("`#[collect]` bound expression must be a string")
                                        }
                                    }
                                } else {
                                    panic!("`#[collect]` encountered an unknown nested item");
                                }
                            }
                            _ => panic!("`#[collect]` encountered an unknown nested item"),
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mode = mode.expect("deriving `Collect` requires a `#[collect(<mode>)]` attribute, where `<mode>` is one of \"require_static\", \"no_drop\", or \"unsafe_drop\"");

    let where_clause = if mode == Mode::RequireStatic {
        quote!(where Self: 'static)
    } else {
        override_bound
            .as_ref()
            .map(|x| {
                x.parse()
                    .expect("`#[collect]` failed to parse explicit trait bound expression")
            })
            .unwrap_or_else(|| quote!())
    };

    let mut errors = vec![];

    let collect_impl = if mode == Mode::RequireStatic {
        s.clone().add_bounds(AddBounds::None).gen_impl(quote! {
            gen unsafe impl gc_arena::Collect for @Self #where_clause {
                #[inline]
                fn needs_trace() -> bool {
                    false
                }
            }
        })
    } else {
        let mut needs_trace_body = TokenStream::new();
        quote!(false).to_tokens(&mut needs_trace_body);

        let mut static_bindings = vec![];

        // Ignore all bindings that have `#[collect(require_static)]`
        // For each binding with `#[collect(require_static)]`, we
        // push a bound of the form `FieldType: 'static` to `static_bindings`,
        // which will be added to the genererated `Collect` impl.
        // The presence of the bound guarantees that the field cannot hold
        // any `Gc` pointers, so it's safe to ignore that field in `needs_trace`
        // and `trace`
        s.filter(|b| {
            let mut static_binding = false;
            let mut seen_collect = false;
            for attr in &b.ast().attrs {
                match attr.parse_meta() {
                    Ok(syn::Meta::List(syn::MetaList { path, nested, .. })) => {
                        if path.is_ident("collect") {
                            if seen_collect {
                                errors.push(
                                    syn::parse::Error::new(
                                        path.span(),
                                        "Cannot specify multiple `#[collect]` attributes!",
                                    )
                                    .to_compile_error(),
                                );
                            }
                            seen_collect = true;

                            let mut valid = false;
                            if let Some(syn::NestedMeta::Meta(syn::Meta::Path(path))) =
                                nested.first()
                            {
                                if path.is_ident("require_static") {
                                    static_binding = true;
                                    static_bindings.push(b.ast().ty.clone());
                                    valid = true;
                                }
                            }

                            if !valid {
                                errors.push(
                                    syn::parse::Error::new(
                                        nested.span(),
                                        "Only `#[collect(require_static)]` is supported on a field",
                                    )
                                    .to_compile_error(),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            !static_binding
        });

        for static_binding in static_bindings {
            s.add_where_predicate(syn::parse_quote! { #static_binding: 'static });
        }

        // `#[collect(require_static)]` only makes sense on fields, not enum
        // variants. Emit an error if it is used in the wrong place
        if let syn::Data::Enum(..) = s.ast().data {
            for v in s.variants() {
                for attr in v.ast().attrs {
                    match attr.parse_meta() {
                        Ok(syn::Meta::List(syn::MetaList { path, nested, .. })) => {
                            if path.is_ident("collect") {
                                errors.push(
                                    syn::parse::Error::new(
                                        nested.span(),
                                        "`#[collect]` is not suppported on enum variants",
                                    )
                                    .to_compile_error(),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // We've already called `s.filter`, so we we won't try to call
        // `needs_trace` for the types of fields that have `#[collect(require_static)]`
        for v in s.variants() {
            for b in v.bindings() {
                let ty = &b.ast().ty;
                // Resolving the span at the call site makes rustc
                // emit a 'the error originates a derive macro note'
                // We only use this span on tokens that need to resolve
                // to items (e.g. `gc_arena::Collect`), so this won't
                // cause any hygiene issues
                let call_span = b.ast().span().resolved_at(Span::call_site());
                quote_spanned!(call_span=>
                    || <#ty as gc_arena::Collect>::needs_trace()
                )
                .to_tokens(&mut needs_trace_body);
            }
        }
        // Likewise, this will skip any fields that have `#[collect(require_static)]`
        let trace_body = s.each(|bi| {
            // See the above call to `needs_trace` for an explanation of this
            let call_span = bi.ast().span().resolved_at(Span::call_site());
            quote_spanned!(call_span=>
                {
                    // Use a temporary variable to ensure that
                    // all tokens in the call to `gc_arena::Collect::trace`
                    // have the same hygiene information. If we used #bi
                    // directly, then we would have a mix of hygiene contexts,
                    // which would cause rustc to produce sub-optimal error
                    // messagse due to its inability to merge the spans.
                    // This is purely for diagnostic purposes, and has no effect
                    // on correctness
                    let bi = #bi;
                    gc_arena::Collect::trace(bi, cc)
                }
            )
        });

        let bounds_type = if override_bound.is_some() {
            AddBounds::None
        } else {
            AddBounds::Fields
        };
        s.clone().add_bounds(bounds_type).gen_impl(quote! {
            gen unsafe impl gc_arena::Collect for @Self #where_clause {
                #[inline]
                fn needs_trace() -> bool {
                    #needs_trace_body
                }

                #[inline]
                fn trace(&self, cc: ::gc_arena::CollectionContext) {
                    match *self { #trace_body }
                }
            }
        })
    };

    let drop_impl = if mode == Mode::NoDrop {
        let mut s = s;
        s.add_bounds(AddBounds::None).gen_impl(quote! {
            gen impl gc_arena::MustNotImplDrop for @Self {}
        })
    } else {
        quote!()
    };

    quote! {
        #collect_impl
        #drop_impl
        #(#errors)*
    }
}

decl_derive!([Collect, attributes(collect)] => collect_derive);
