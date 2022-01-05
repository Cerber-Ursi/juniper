//! Code generation for [GraphQL object][1].
//!
//! [1]: https://spec.graphql.org/June2018/#sec-Objects

pub mod attr;
pub mod derive;

use std::{any::TypeId, collections::HashSet, convert::TryInto as _, marker::PhantomData};

use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    parse_quote,
    spanned::Spanned as _,
    token,
};

use crate::{
    common::{
        field, gen,
        parse::{
            attr::{err, OptionExt as _},
            ParseBufferExt as _, TypeExt,
        },
        scalar,
    },
    util::{filter_attrs, get_doc_comment, span_container::SpanContainer, RenameRule},
};
use syn::ext::IdentExt;

/// Available arguments behind `#[graphql]` (or `#[graphql_object]`) attribute
/// when generating code for [GraphQL object][1] type.
///
/// [1]: https://spec.graphql.org/June2018/#sec-Objects
#[derive(Debug, Default)]
pub(crate) struct Attr {
    /// Explicitly specified name of this [GraphQL object][1] type.
    ///
    /// If [`None`], then Rust type name is used by default.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) name: Option<SpanContainer<String>>,

    /// Explicitly specified [description][2] of this [GraphQL object][1] type.
    ///
    /// If [`None`], then Rust doc comment is used as [description][2], if any.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    /// [2]: https://spec.graphql.org/June2018/#sec-Descriptions
    pub(crate) description: Option<SpanContainer<String>>,

    /// Explicitly specified type of [`Context`] to use for resolving this
    /// [GraphQL object][1] type with.
    ///
    /// If [`None`], then unit type `()` is assumed as a type of [`Context`].
    ///
    /// [`Context`]: juniper::Context
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) context: Option<SpanContainer<syn::Type>>,

    /// Explicitly specified type (or type parameter with its bounds) of
    /// [`ScalarValue`] to use for resolving this [GraphQL object][1] type with.
    ///
    /// If [`None`], then generated code will be generic over any
    /// [`ScalarValue`] type, which, in turn, requires all [object][1] fields to
    /// be generic over any [`ScalarValue`] type too. That's why this type
    /// should be specified only if one of the variants implements
    /// [`GraphQLType`] in a non-generic way over [`ScalarValue`] type.
    ///
    /// [`GraphQLType`]: juniper::GraphQLType
    /// [`ScalarValue`]: juniper::ScalarValue
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) scalar: Option<SpanContainer<scalar::AttrValue>>,

    /// Explicitly specified [GraphQL interfaces][2] this [GraphQL object][1]
    /// type implements.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    /// [2]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub(crate) interfaces: HashSet<SpanContainer<syn::Type>>,

    /// Explicitly specified [`RenameRule`] for all fields of this
    /// [GraphQL object][1] type.
    ///
    /// If [`None`] then the default rule will be [`RenameRule::CamelCase`].
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) rename_fields: Option<SpanContainer<RenameRule>>,

    /// Indicator whether the generated code is intended to be used only inside
    /// the [`juniper`] library.
    pub(crate) is_internal: bool,
}

impl Parse for Attr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut out = Self::default();
        while !input.is_empty() {
            let ident = input.parse_any_ident()?;
            match ident.to_string().as_str() {
                "name" => {
                    input.parse::<token::Eq>()?;
                    let name = input.parse::<syn::LitStr>()?;
                    out.name
                        .replace(SpanContainer::new(
                            ident.span(),
                            Some(name.span()),
                            name.value(),
                        ))
                        .none_or_else(|_| err::dup_arg(&ident))?
                }
                "desc" | "description" => {
                    input.parse::<token::Eq>()?;
                    let desc = input.parse::<syn::LitStr>()?;
                    out.description
                        .replace(SpanContainer::new(
                            ident.span(),
                            Some(desc.span()),
                            desc.value(),
                        ))
                        .none_or_else(|_| err::dup_arg(&ident))?
                }
                "ctx" | "context" | "Context" => {
                    input.parse::<token::Eq>()?;
                    let ctx = input.parse::<syn::Type>()?;
                    out.context
                        .replace(SpanContainer::new(ident.span(), Some(ctx.span()), ctx))
                        .none_or_else(|_| err::dup_arg(&ident))?
                }
                "scalar" | "Scalar" | "ScalarValue" => {
                    input.parse::<token::Eq>()?;
                    let scl = input.parse::<scalar::AttrValue>()?;
                    out.scalar
                        .replace(SpanContainer::new(ident.span(), Some(scl.span()), scl))
                        .none_or_else(|_| err::dup_arg(&ident))?
                }
                "impl" | "implements" | "interfaces" => {
                    input.parse::<token::Eq>()?;
                    for iface in input.parse_maybe_wrapped_and_punctuated::<
                        syn::Type, token::Bracket, token::Comma,
                    >()? {
                        let iface_span = iface.span();
                        out
                            .interfaces
                            .replace(SpanContainer::new(ident.span(), Some(iface_span), iface))
                            .none_or_else(|_| err::dup_arg(iface_span))?;
                    }
                }
                "rename_all" => {
                    input.parse::<token::Eq>()?;
                    let val = input.parse::<syn::LitStr>()?;
                    out.rename_fields
                        .replace(SpanContainer::new(
                            ident.span(),
                            Some(val.span()),
                            val.try_into()?,
                        ))
                        .none_or_else(|_| err::dup_arg(&ident))?;
                }
                "internal" => {
                    out.is_internal = true;
                }
                name => {
                    return Err(err::unknown_arg(&ident, name));
                }
            }
            input.try_parse::<token::Comma>()?;
        }
        Ok(out)
    }
}

impl Attr {
    /// Tries to merge two [`Attr`]s into a single one, reporting about
    /// duplicates, if any.
    fn try_merge(self, mut another: Self) -> syn::Result<Self> {
        Ok(Self {
            name: try_merge_opt!(name: self, another),
            description: try_merge_opt!(description: self, another),
            context: try_merge_opt!(context: self, another),
            scalar: try_merge_opt!(scalar: self, another),
            interfaces: try_merge_hashset!(interfaces: self, another => span_joined),
            rename_fields: try_merge_opt!(rename_fields: self, another),
            is_internal: self.is_internal || another.is_internal,
        })
    }

    /// Parses [`Attr`] from the given multiple `name`d [`syn::Attribute`]s
    /// placed on a struct or impl block definition.
    pub(crate) fn from_attrs(name: &str, attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut attr = filter_attrs(name, attrs)
            .map(|attr| attr.parse_args())
            .try_fold(Self::default(), |prev, curr| prev.try_merge(curr?))?;

        if attr.description.is_none() {
            attr.description = get_doc_comment(attrs);
        }

        Ok(attr)
    }
}

/// Definition of [GraphQL object][1] for code generation.
///
/// [1]: https://spec.graphql.org/June2018/#sec-Objects
#[derive(Debug)]
pub(crate) struct Definition<Operation: ?Sized> {
    /// Name of this [GraphQL object][1] in GraphQL schema.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) name: String,

    /// Rust type that this [GraphQL object][1] is represented with.
    ///
    /// It should contain all its generics, if any.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) ty: syn::Type,

    /// Generics of the Rust type that this [GraphQL object][1] is implemented
    /// for.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) generics: syn::Generics,

    /// Description of this [GraphQL object][1] to put into GraphQL schema.
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) description: Option<String>,

    /// Rust type of [`Context`] to generate [`GraphQLType`] implementation with
    /// for this [GraphQL object][1].
    ///
    /// [`GraphQLType`]: juniper::GraphQLType
    /// [`Context`]: juniper::Context
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) context: syn::Type,

    /// [`ScalarValue`] parametrization to generate [`GraphQLType`]
    /// implementation with for this [GraphQL object][1].
    ///
    /// [`GraphQLType`]: juniper::GraphQLType
    /// [`ScalarValue`]: juniper::ScalarValue
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    pub(crate) scalar: scalar::Type,

    /// Defined [GraphQL fields][2] of this [GraphQL object][1].
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    /// [2]: https://spec.graphql.org/June2018/#sec-Language.Fields
    pub(crate) fields: Vec<field::Definition>,

    /// [GraphQL interfaces][2] implemented by this [GraphQL object][1].
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    /// [2]: https://spec.graphql.org/June2018/#sec-Interfaces
    pub(crate) interfaces: HashSet<syn::Type>,

    /// [GraphQL operation][1] this [`Definition`] should generate code for.
    ///
    /// Either [GraphQL query][2] or [GraphQL subscription][3].
    ///
    /// [1]: https://spec.graphql.org/June2018/#sec-Language.Operations
    /// [2]: https://spec.graphql.org/June2018/#sec-Query
    /// [3]: https://spec.graphql.org/June2018/#sec-Subscription
    pub(crate) _operation: PhantomData<Box<Operation>>,
}

impl<Operation: ?Sized + 'static> Definition<Operation> {
    /// Returns prepared [`syn::Generics::split_for_impl`] for [`GraphQLType`]
    /// trait (and similar) implementation of this [GraphQL object][1].
    ///
    /// If `for_async` is `true`, then additional predicates are added to suit
    /// the [`GraphQLAsyncValue`] trait (and similar) requirements.
    ///
    /// [`GraphQLAsyncValue`]: juniper::GraphQLAsyncValue
    /// [`GraphQLType`]: juniper::GraphQLType
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    pub(crate) fn impl_generics(&self, for_async: bool) -> (TokenStream, Option<syn::WhereClause>) {
        let mut generics = self.generics.clone();

        let scalar = &self.scalar;
        if scalar.is_implicit_generic() {
            generics.params.push(parse_quote! { #scalar });
        }
        if scalar.is_generic() {
            generics
                .make_where_clause()
                .predicates
                .push(parse_quote! { #scalar: ::juniper::ScalarValue });
        }
        if let Some(bound) = scalar.bounds() {
            generics.make_where_clause().predicates.push(bound);
        }

        if for_async {
            let self_ty = if self.generics.lifetimes().next().is_some() {
                let mut lifetimes = vec![];

                // Modify lifetime names to omit "lifetime name `'a` shadows a
                // lifetime name that is already in scope" error.
                let mut ty = self.ty.clone();
                ty.lifetimes_iter_mut(&mut |lt| {
                    let ident = lt.ident.unraw();
                    lt.ident = format_ident!("__fa__{}", ident);
                    lifetimes.push(lt.clone());
                });

                quote! { for<#( #lifetimes ),*> #ty }
            } else {
                quote! { Self }
            };
            generics
                .make_where_clause()
                .predicates
                .push(parse_quote! { #self_ty: Sync });

            if scalar.is_generic() {
                generics
                    .make_where_clause()
                    .predicates
                    .push(parse_quote! { #scalar: Send + Sync });
            }
        }

        let (impl_generics, _, where_clause) = generics.split_for_impl();
        (quote! { #impl_generics }, where_clause.cloned())
    }

    /// Returns generated code implementing [`marker::IsOutputType`] trait for
    /// this [GraphQL object][1].
    ///
    /// [`marker::IsOutputType`]: juniper::marker::IsOutputType
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    pub(crate) fn impl_output_type_tokens(&self) -> TokenStream {
        let scalar = &self.scalar;

        let (impl_generics, where_clause) = self.impl_generics(false);
        let ty = &self.ty;

        let coerce_result = TypeId::of::<Operation>() != TypeId::of::<Query>();
        let fields_marks = self
            .fields
            .iter()
            .map(|f| f.method_mark_tokens(coerce_result, scalar));

        let interface_tys = self.interfaces.iter();

        quote! {
            #[automatically_derived]
            impl#impl_generics ::juniper::marker::IsOutputType<#scalar> for #ty #where_clause
            {
                fn mark() {
                    #( #fields_marks )*
                    #( <#interface_tys as ::juniper::marker::IsOutputType<#scalar>>::mark(); )*
                }
            }
        }
    }

    /// Returns generated code implementing [`BaseType`], [`BaseSubTypes`],
    /// [`WrappedType`] and [`Fields`] traits for this [GraphQL object][1].
    ///
    /// [`BaseSubTypes`]: juniper::macros::reflection::BaseSubTypes
    /// [`BaseType`]: juniper::macros::reflection::BaseType
    /// [`Fields`]: juniper::macros::reflection::Fields
    /// [`WrappedType`]: juniper::macros::reflection::WrappedType
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    pub(crate) fn impl_traits_for_reflection_tokens(&self) -> TokenStream {
        let scalar = &self.scalar;
        let name = &self.name;
        let (impl_generics, where_clause) = self.impl_generics(false);
        let ty = &self.ty;
        let fields = self.fields.iter().map(|f| &f.name);

        quote! {
            #[automatically_derived]
            impl#impl_generics ::juniper::macros::reflection::BaseType<#scalar>
                for #ty
                #where_clause
            {
                const NAME: ::juniper::macros::reflection::Type = #name;
            }

            #[automatically_derived]
            impl#impl_generics ::juniper::macros::reflection::BaseSubTypes<#scalar>
                for #ty
                #where_clause
            {
                const NAMES: ::juniper::macros::reflection::Types =
                    &[<Self as ::juniper::macros::reflection::BaseType<#scalar>>::NAME];
            }

            #[automatically_derived]
            impl#impl_generics ::juniper::macros::reflection::WrappedType<#scalar>
                for #ty
                #where_clause
            {
                const VALUE: ::juniper::macros::reflection::WrappedValue = 1;
            }

            #[automatically_derived]
            impl#impl_generics ::juniper::macros::reflection::Fields<#scalar>
                for #ty
                #where_clause
            {
                const NAMES: ::juniper::macros::reflection::Names = &[#(#fields),*];
            }
        }
    }

    /// Returns generated code implementing [`GraphQLType`] trait for this
    /// [GraphQL object][1].
    ///
    /// [`GraphQLType`]: juniper::GraphQLType
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    pub(crate) fn impl_graphql_type_tokens(&self) -> TokenStream {
        let scalar = &self.scalar;

        let (impl_generics, where_clause) = self.impl_generics(false);
        let ty = &self.ty;

        let name = &self.name;
        let description = self
            .description
            .as_ref()
            .map(|desc| quote! { .description(#desc) });

        let extract_stream_type = TypeId::of::<Operation>() != TypeId::of::<Query>();
        let fields_meta = self
            .fields
            .iter()
            .map(|f| f.method_meta_tokens(extract_stream_type.then(|| scalar)));

        // Sorting is required to preserve/guarantee the order of interfaces registered in schema.
        let mut interface_tys: Vec<_> = self.interfaces.iter().collect();
        interface_tys.sort_unstable_by(|a, b| {
            let (a, b) = (quote!(#a).to_string(), quote!(#b).to_string());
            a.cmp(&b)
        });
        let interfaces = (!interface_tys.is_empty()).then(|| {
            quote! {
                .interfaces(&[
                    #( registry.get_type::<#interface_tys>(info), )*
                ])
            }
        });

        quote! {
            #[automatically_derived]
            impl#impl_generics ::juniper::GraphQLType<#scalar> for #ty #where_clause
            {
                fn name(_ : &Self::TypeInfo) -> Option<&'static str> {
                    Some(#name)
                }

                fn meta<'r>(
                    info: &Self::TypeInfo,
                    registry: &mut ::juniper::Registry<'r, #scalar>
                ) -> ::juniper::meta::MetaType<'r, #scalar>
                where #scalar: 'r,
                {
                    let fields = [
                        #( #fields_meta, )*
                    ];
                    registry.build_object_type::<#ty>(info, &fields)
                        #description
                        #interfaces
                        .into_meta()
                }
            }
        }
    }
}

/// [GraphQL query operation][2] of the [`Definition`] to generate code for.
///
/// [2]: https://spec.graphql.org/June2018/#sec-Query
struct Query;

impl ToTokens for Definition<Query> {
    fn to_tokens(&self, into: &mut TokenStream) {
        self.impl_graphql_object_tokens().to_tokens(into);
        self.impl_output_type_tokens().to_tokens(into);
        self.impl_graphql_type_tokens().to_tokens(into);
        self.impl_graphql_value_tokens().to_tokens(into);
        self.impl_graphql_value_async_tokens().to_tokens(into);
        self.impl_as_dyn_graphql_value_tokens().to_tokens(into);
        self.impl_traits_for_reflection_tokens().to_tokens(into);
        self.impl_field_meta_tokens().to_tokens(into);
        self.impl_field_tokens().to_tokens(into);
        self.impl_async_field_tokens().to_tokens(into);
    }
}

impl Definition<Query> {
    /// Returns generated code implementing [`GraphQLObject`] trait for this
    /// [GraphQL object][1].
    ///
    /// [`GraphQLObject`]: juniper::GraphQLObject
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_graphql_object_tokens(&self) -> TokenStream {
        let scalar = &self.scalar;

        let (impl_generics, where_clause) = self.impl_generics(false);
        let ty = &self.ty;

        let interface_tys = self.interfaces.iter();
        // TODO: Make it work by repeating `sa::assert_type_ne_all!` expansion,
        //       but considering generics.
        //let interface_tys: Vec<_> = self.interfaces.iter().collect();
        //let all_interfaces_unique = (interface_tys.len() > 1).then(|| {
        //    quote! { ::juniper::sa::assert_type_ne_all!(#( #interface_tys ),*); }
        //});

        quote! {
            #[automatically_derived]
            impl#impl_generics ::juniper::marker::GraphQLObject<#scalar> for #ty #where_clause
            {
                fn mark() {
                    #( <#interface_tys as ::juniper::marker::GraphQLInterface<#scalar>>::mark(); )*
                }
            }
        }
    }

    /// Returns generated code implementing [`FieldMeta`] traits for each field
    /// of this [GraphQL object][1].
    ///
    /// [`FieldMeta`]: juniper::FieldMeta
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_field_meta_tokens(&self) -> TokenStream {
        let impl_ty = &self.ty;
        let scalar = &self.scalar;
        let context = &self.context;
        let (impl_generics, where_clause) = self.impl_generics(false);

        self.fields
            .iter()
            .map(|field| {
                let (name, ty) = (&field.name, field.ty.clone());

                let arguments = field
                    .arguments
                    .as_ref()
                    .iter()
                    .flat_map(|vec| vec.iter())
                    .filter_map(|arg| match arg {
                        field::MethodArgument::Regular(arg) => {
                            let (name, ty) = (&arg.name, &arg.ty);

                            Some(quote! {(
                                #name,
                                <#ty as ::juniper::macros::reflection::BaseType<#scalar>>::NAME,
                                <#ty as ::juniper::macros::reflection::WrappedType<#scalar>>::VALUE,
                            )})
                        }
                        field::MethodArgument::Executor | field::MethodArgument::Context(_) => None,
                    })
                    .collect::<Vec<_>>();

                quote! {
                    #[allow(deprecated, non_snake_case)]
                    #[automatically_derived]
                    impl #impl_generics ::juniper::macros::reflection::FieldMeta<
                        #scalar,
                        { ::juniper::macros::reflection::fnv1a128(#name) }
                    > for #impl_ty
                        #where_clause
                    {
                        type Context = #context;
                        type TypeInfo = ();
                        const TYPE: ::juniper::macros::reflection::Type =
                            <#ty as ::juniper::macros::reflection::BaseType<#scalar>>::NAME;
                        const SUB_TYPES: ::juniper::macros::reflection::Types =
                            <#ty as ::juniper::macros::reflection::BaseSubTypes<#scalar>>::NAMES;
                        const WRAPPED_VALUE: juniper::macros::reflection::WrappedValue =
                            <#ty as ::juniper::macros::reflection::WrappedType<#scalar>>::VALUE;
                        const ARGUMENTS: &'static [(
                            ::juniper::macros::reflection::Name,
                            ::juniper::macros::reflection::Type,
                            ::juniper::macros::reflection::WrappedValue,
                        )] = &[#(#arguments,)*];
                    }
                }
            })
            .collect()
    }

    /// Returns generated code implementing [`Field`] trait for each field of
    /// this [GraphQL object][1].
    ///
    /// [`Field`]: juniper::Field
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_field_tokens(&self) -> TokenStream {
        let (impl_ty, scalar) = (&self.ty, &self.scalar);
        let (impl_generics, where_clause) = self.impl_generics(false);

        self.fields
            .iter()
            .filter_map(|field| {
                if field.is_async {
                    return None;
                }

                let (name, mut res_ty, ident) = (&field.name, field.ty.clone(), &field.ident);

                let res = if field.is_method() {
                    let args = field
                        .arguments
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|arg| arg.method_resolve_field_tokens(scalar, false));

                    let rcv = field.has_receiver.then(|| {
                        quote! { self, }
                    });

                    quote! { Self::#ident(#rcv #( #args ),*) }
                } else {
                    res_ty = parse_quote! { _ };
                    quote! { &self.#ident }
                };

                let resolving_code = gen::sync_resolving_code();

                Some(quote! {
                    #[allow(deprecated, non_snake_case)]
                    #[automatically_derived]
                    impl #impl_generics ::juniper::macros::reflection::Field<
                        #scalar,
                        { ::juniper::macros::reflection::fnv1a128(#name) }
                    > for #impl_ty
                        #where_clause
                    {
                        fn call(
                            &self,
                            info: &Self::TypeInfo,
                            args: &::juniper::Arguments<#scalar>,
                            executor: &::juniper::Executor<Self::Context, #scalar>,
                        ) -> ::juniper::ExecutionResult<#scalar> {
                            let res: #res_ty = #res;
                            #resolving_code
                        }
                    }
                })
            })
            .collect()
    }

    /// Returns generated code implementing [`AsyncField`] trait for each field
    /// of this [GraphQL object][1].
    ///
    /// [`AsyncField`]: juniper::AsyncField
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_async_field_tokens(&self) -> TokenStream {
        let (impl_ty, scalar) = (&self.ty, &self.scalar);
        let (impl_generics, where_clause) = self.impl_generics(true);

        self.fields
            .iter()
            .map(|field| {
                let (name, mut res_ty, ident) = (&field.name, field.ty.clone(), &field.ident);

                let mut res = if field.is_method() {
                    let args = field
                        .arguments
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|arg| arg.method_resolve_field_tokens(scalar, true));

                    let rcv = field.has_receiver.then(|| {
                        quote! { self, }
                    });

                    quote! { Self::#ident(#rcv #( #args ),*) }
                } else {
                    res_ty = parse_quote! { _ };
                    quote! { &self.#ident }
                };
                if !field.is_async {
                    res = quote! { ::juniper::futures::future::ready(#res) };
                }

                let resolving_code = gen::async_resolving_code(Some(&res_ty));

                quote! {
                    #[allow(deprecated, non_snake_case)]
                    #[automatically_derived]
                    impl #impl_generics ::juniper::macros::reflection::AsyncField<
                        #scalar,
                        { ::juniper::macros::reflection::fnv1a128(#name) }
                    > for #impl_ty
                        #where_clause
                    {
                        fn call<'b>(
                            &'b self,
                            info: &'b Self::TypeInfo,
                            args: &'b ::juniper::Arguments<#scalar>,
                            executor: &'b ::juniper::Executor<Self::Context, #scalar>,
                        ) -> ::juniper::BoxFuture<'b, ::juniper::ExecutionResult<#scalar>> {
                            let fut = #res;
                            #resolving_code
                        }
                    }
                }
            })
            .collect()
    }

    /// Returns generated code implementing [`GraphQLValue`] trait for this
    /// [GraphQL object][1].
    ///
    /// [`GraphQLValue`]: juniper::GraphQLValue
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_graphql_value_tokens(&self) -> TokenStream {
        let scalar = &self.scalar;
        let context = &self.context;

        let (impl_generics, where_clause) = self.impl_generics(false);
        let ty = &self.ty;
        let ty_name = ty.to_token_stream().to_string();

        let name = &self.name;

        let fields_resolvers = self.fields.iter().filter_map(|f| {
            if f.is_async {
                return None;
            }

            let name = &f.name;
            Some(quote! {
                #name => {
                    ::juniper::macros::reflection::Field::<
                        #scalar,
                        { ::juniper::macros::reflection::fnv1a128(#name) }
                    >::call(self, info, args, executor)
                }
            })
        });

        let async_fields_err = {
            let names = self
                .fields
                .iter()
                .filter_map(|f| f.is_async.then(|| f.name.as_str()))
                .collect::<Vec<_>>();
            (!names.is_empty()).then(|| {
                field::Definition::method_resolve_field_err_async_field_tokens(
                    &names, scalar, &ty_name,
                )
            })
        };
        let no_field_err =
            field::Definition::method_resolve_field_err_no_field_tokens(scalar, &ty_name);

        quote! {
            #[allow(deprecated)]
            #[automatically_derived]
            impl#impl_generics ::juniper::GraphQLValue<#scalar> for #ty #where_clause
            {
                type Context = #context;
                type TypeInfo = ();

                fn type_name<'__i>(&self, info: &'__i Self::TypeInfo) -> Option<&'__i str> {
                    <Self as ::juniper::GraphQLType<#scalar>>::name(info)
                }

                fn resolve_field(
                    &self,
                    info: &Self::TypeInfo,
                    field: &str,
                    args: &::juniper::Arguments<#scalar>,
                    executor: &::juniper::Executor<Self::Context, #scalar>,
                ) -> ::juniper::ExecutionResult<#scalar> {
                    match field {
                        #( #fields_resolvers )*
                        #async_fields_err
                        _ => #no_field_err,
                    }
                }

                fn concrete_type_name(
                    &self,
                    _: &Self::Context,
                    _: &Self::TypeInfo,
                ) -> String {
                    #name.to_string()
                }
            }
        }
    }

    /// Returns generated code implementing [`GraphQLValueAsync`] trait for this
    /// [GraphQL object][1].
    ///
    /// [`GraphQLValueAsync`]: juniper::GraphQLValueAsync
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_graphql_value_async_tokens(&self) -> TokenStream {
        let scalar = &self.scalar;

        let (impl_generics, where_clause) = self.impl_generics(true);
        let ty = &self.ty;
        let ty_name = ty.to_token_stream().to_string();

        let fields_resolvers = self.fields.iter().map(|f| {
            let name = &f.name;
            quote! {
                #name => {
                    ::juniper::macros::reflection::AsyncField::<
                        #scalar,
                        { ::juniper::macros::reflection::fnv1a128(#name) }
                    >::call(self, info, args, executor)
                }
            }
        });

        let no_field_err =
            field::Definition::method_resolve_field_err_no_field_tokens(scalar, &ty_name);

        quote! {
            #[allow(deprecated, non_snake_case)]
            #[automatically_derived]
            impl#impl_generics ::juniper::GraphQLValueAsync<#scalar> for #ty #where_clause
            {
                fn resolve_field_async<'b>(
                    &'b self,
                    info: &'b Self::TypeInfo,
                    field: &'b str,
                    args: &'b ::juniper::Arguments<#scalar>,
                    executor: &'b ::juniper::Executor<Self::Context, #scalar>,
                ) -> ::juniper::BoxFuture<'b, ::juniper::ExecutionResult<#scalar>> {
                    match field {
                        #( #fields_resolvers )*
                        _ => Box::pin(async move { #no_field_err }),
                    }
                }
            }
        }
    }

    /// Returns generated code implementing [`AsDynGraphQLValue`] trait for this
    /// [GraphQL object][1].
    ///
    /// [`AsDynGraphQLValue`]: juniper::AsDynGraphQLValue
    /// [1]: https://spec.graphql.org/June2018/#sec-Objects
    #[must_use]
    fn impl_as_dyn_graphql_value_tokens(&self) -> Option<TokenStream> {
        if self.interfaces.is_empty() {
            return None;
        }

        let scalar = &self.scalar;

        let (impl_generics, where_clause) = self.impl_generics(true);
        let ty = &self.ty;

        Some(quote! {
            #[allow(non_snake_case)]
            #[automatically_derived]
            impl#impl_generics ::juniper::AsDynGraphQLValue<#scalar> for #ty #where_clause
            {
                type Context = <Self as ::juniper::GraphQLValue<#scalar>>::Context;
                type TypeInfo = <Self as ::juniper::GraphQLValue<#scalar>>::TypeInfo;

                fn as_dyn_graphql_value(
                    &self,
                ) -> &::juniper::DynGraphQLValue<#scalar, Self::Context, Self::TypeInfo> {
                    self
                }

                fn as_dyn_graphql_value_async(
                    &self,
                ) -> &::juniper::DynGraphQLValueAsync<#scalar, Self::Context, Self::TypeInfo> {
                    self
                }
            }
        })
    }
}