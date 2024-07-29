use std::{
    collections::{HashMap, HashSet},
    ops::{Deref, Not},
    path::PathBuf,
    rc::Rc,
};

pub mod cfg_file;
pub mod declare_locales;
pub mod error;
pub mod interpolate;
pub mod key;
pub mod locale;
pub mod parsed_value;
pub mod plural;
pub mod tracking;
pub mod warning;

use cfg_file::ConfigFile;
use error::{Error, Result};
use interpolate::{create_empty_type, Interpolation};
use key::{Key, KeyPath};
use locale::{Locale, LocaleValue};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};

use crate::load_locales::parsed_value::ParsedValue;

use self::{
    locale::{BuildersKeys, BuildersKeysInner, LocalesOrNamespaces, Namespace},
    warning::generate_warnings,
};

/// Steps:
///
/// 1: Locate and parse the manifest (`ConfigFile::new`)
/// 2: parse each locales/namespaces files (`LocalesOrNamespaces::new`)
/// 3: Resolve foreign keys (`ParsedValue::resolve_foreign_keys`)
/// 4: check the locales: (`Locale::check_locales`)
/// 4.1: get interpolations keys of the default, meaning all variables/components/plurals of the default locale (`Locale::make_builder_keys`)
/// 4.2: in the process reduce all values and check for default in the default locale
/// 4.3: then merge all other locales in the default locale keys, reducing all values in the process (`Locale::merge`)
/// 4.4: discard any surplus key and emit a warning
/// 5: generate code (and warnings)
pub fn load_locales() -> Result<TokenStream> {
    let mut cargo_manifest_dir: PathBuf = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(Error::CargoDirEnvNotPresent)?
        .into();

    let cfg_file = ConfigFile::new(&mut cargo_manifest_dir)?;
    let mut locales = LocalesOrNamespaces::new(&mut cargo_manifest_dir, &cfg_file)?;

    load_locales_inner(&cfg_file, &mut locales)
}

fn load_locales_inner(
    cfg_file: &ConfigFile,
    locales: &mut LocalesOrNamespaces,
) -> Result<TokenStream> {
    ParsedValue::resolve_foreign_keys(locales, &cfg_file.default)?;

    let keys = Locale::check_locales(locales)?;

    let enum_ident = syn::Ident::new("Locale", Span::call_site());
    let keys_ident = syn::Ident::new("I18nKeys", Span::call_site());

    let locale_type = create_locale_type(keys, cfg_file, &keys_ident, &enum_ident);
    let locale_enum = create_locales_enum(
        &enum_ident,
        &keys_ident,
        &cfg_file.default,
        &cfg_file.locales,
    );

    let warnings = generate_warnings();

    let file_tracking = tracking::generate_file_tracking();

    let mut macros_reexport = vec![quote!(t), quote!(td), quote!(tu)];
    if cfg!(feature = "interpolate_display") {
        macros_reexport.extend([
            quote!(t_string),
            quote!(tu_string),
            quote!(t_display),
            quote!(tu_display),
            quote!(td_string),
            quote!(td_display),
        ]);
    }

    let island_or_component = if cfg!(feature = "experimental-islands") {
        macros_reexport.push(quote!(ti));
        quote!(island)
    } else {
        quote!(component)
    };

    let macros_reexport = quote!(pub use leptos_i18n::{#(#macros_reexport,)*};);

    Ok(quote! {
        pub mod i18n {
            #file_tracking

            #locale_enum

            #locale_type

            #[inline]
            pub fn use_i18n() -> leptos_i18n::I18nContext<#enum_ident> {
                leptos_i18n::use_i18n_context()
            }

            #[inline]
            pub fn provide_i18n_context() -> leptos_i18n::I18nContext<#enum_ident> {
                leptos_i18n::provide_i18n_context()
            }

            mod provider {
                #[leptos::#island_or_component]
                #[allow(non_snake_case)]
                pub fn I18nContextProvider(children: leptos::Children) -> impl leptos::IntoView {
                    super::provide_i18n_context();
                    children()
                }
            }

            pub use provider::I18nContextProvider;

            #macros_reexport

            #warnings
        }
    })
}

fn create_locales_enum(
    enum_ident: &syn::Ident,
    keys_ident: &syn::Ident,
    default: &Key,
    locales: &[Rc<Key>],
) -> TokenStream {
    let as_str_match_arms = locales
        .iter()
        .map(|key| (&key.ident, &key.name))
        .map(|(variant, locale)| quote!(#enum_ident::#variant => #locale))
        .collect::<Vec<_>>();

    let from_str_match_arms = locales
        .iter()
        .map(|key| (&key.ident, &key.name))
        .map(|(variant, locale)| quote!(#locale => Some(#enum_ident::#variant)))
        .collect::<Vec<_>>();

    let from_parts_match_arms = locales
        .iter()
        .map(|key| (&key.ident, &key.name))
        .map(|(variant, locale)| {
            let parts = locale.split('-').map(str::trim);
            quote!(&[#(#parts | "*",)* ref rest @ ..] if rest.iter().all(|p| *p == "*") => Some(#enum_ident::#variant))
        })
        .collect::<Vec<_>>();

    let derives = if cfg!(feature = "serde") {
        quote!(#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)])
    } else {
        quote!(#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)])
    };

    quote! {
        #derives
        #[allow(non_camel_case_types)]
        pub enum #enum_ident {
            #(#locales,)*
        }

        impl Default for #enum_ident {
            fn default() -> Self {
                #enum_ident::#default
            }
        }

        impl leptos_i18n::Locale for #enum_ident {
            type Keys = #keys_ident;

            fn as_str(self) -> &'static str {
                match self {
                    #(#as_str_match_arms,)*
                }
            }

            fn from_parts(s: &[&str]) -> Option<Self> {
                match s {
                    #(#from_parts_match_arms,)*
                    _ => None
                }
            }

            fn from_str(s: &str) -> Option<Self> {
                match s.trim() {
                    #(#from_str_match_arms,)*
                    _ => None
                }
            }
        }
    }
}

struct Subkeys<'a> {
    original_key: Rc<Key>,
    key: syn::Ident,
    mod_key: syn::Ident,
    locales: &'a [Locale],
    keys: &'a BuildersKeysInner,
}

impl<'a> Subkeys<'a> {
    pub fn new(key: Rc<Key>, locales: &'a [Locale], keys: &'a BuildersKeysInner) -> Self {
        let mod_key = format_ident!("sk_{}", key.ident);
        let new_key = format_ident!("{}_subkeys", key.ident);
        Subkeys {
            original_key: key,
            key: new_key,
            mod_key,
            locales,
            keys,
        }
    }
}

fn get_default_match(
    default_locale: &Key,
    top_locales: &HashSet<&Key>,
    locales: &[Locale],
    enum_ident: &syn::Ident,
) -> TokenStream {
    let current_keys = locales
        .iter()
        .map(|locale| &*locale.top_locale_name)
        .collect();
    let missing_keys = top_locales.difference(&current_keys);
    quote!(#enum_ident::#default_locale #(| #enum_ident::#missing_keys)*)
}

#[allow(clippy::too_many_arguments)]
fn create_locale_type_inner(
    default_locale: &Key,
    type_ident: &syn::Ident,
    enum_ident: &syn::Ident,
    top_locales: &HashSet<&Key>,
    locales: &[Locale],
    keys: &HashMap<Rc<Key>, LocaleValue>,
    is_namespace: bool,
    key_path: &mut KeyPath,
) -> TokenStream {
    let default_match = get_default_match(default_locale, top_locales, locales, enum_ident);

    let string_keys = keys
        .iter()
        .filter(|(_, value)| matches!(value, LocaleValue::Value(None)))
        .map(|(key, _)| key)
        .collect::<Vec<_>>();

    let string_fields = string_keys
        .iter()
        .map(|key| quote!(pub #key: &'static str))
        .collect::<Vec<_>>();

    let subkeys = keys
        .iter()
        .filter_map(|(key, value)| match value {
            LocaleValue::Subkeys { locales, keys } => {
                Some(Subkeys::new(key.clone(), locales, keys))
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    let subkeys_ts = subkeys.iter().map(|sk| {
        let subkey_mod_ident = &sk.mod_key;
        key_path.push_key(sk.original_key.clone());
        let subkey_impl = create_locale_type_inner(
            default_locale,
            &sk.key,
            enum_ident,
            top_locales,
            sk.locales,
            &sk.keys.0,
            true,
            key_path,
        );
        key_path.pop_key();
        quote! {
            pub mod #subkey_mod_ident {
                use super::#enum_ident;

                #subkey_impl
            }
        }
    });

    let subkeys_fields = subkeys.iter().map(|sk| {
        let original_key = &sk.original_key;
        let key = &sk.key;
        let mod_ident = &sk.mod_key;
        quote!(pub #original_key: subkeys::#mod_ident::#key)
    });

    let subkeys_field_new = subkeys
        .iter()
        .map(|sk| {
            let original_key = &sk.original_key;
            let key = &sk.key;
            let mod_ident = &sk.mod_key;
            quote!(#original_key: subkeys::#mod_ident::#key::new(_locale))
        })
        .collect::<Vec<_>>();

    let subkeys_module = subkeys.is_empty().not().then(move || {
        quote! {
            #[doc(hidden)]
            pub mod subkeys {
                use super::#enum_ident;

                #(
                    #subkeys_ts
                )*
            }
        }
    });

    let builders = keys
        .iter()
        .filter_map(|(key, value)| match value {
            LocaleValue::Value(None) | LocaleValue::Subkeys { .. } => None,
            LocaleValue::Value(Some(keys)) => Some((
                key,
                Interpolation::new(key, enum_ident, keys, locales, &default_match, key_path),
            )),
        })
        .collect::<Vec<_>>();

    let builder_fields = builders.iter().map(|(key, inter)| {
        let inter_ident = &inter.default_generic_ident;
        quote!(pub #key: builders::#inter_ident)
    });

    let init_builder_fields: Vec<TokenStream> = builders
        .iter()
        .map(|(key, inter)| {
            let ident = &inter.ident;
            quote!(#key: builders::#ident::new(_locale))
        })
        .collect();

    let default_locale = locales.first().unwrap();

    let new_match_arms = locales.iter().enumerate().map(|(i, locale)| {
        let filled_string_fields = string_keys.iter().filter_map(|&key| {
            if cfg!(feature = "show_keys_only") {
                let key_str = key_path.to_string_with_key(key);
                return Some(quote!(#key: #key_str));
            }
            match locale.keys.get(key) {
                Some(ParsedValue::String(str_value)) => Some(quote!(#key: #str_value)),
                _ => {
                    let str_value = default_locale
                        .keys
                        .get(key)
                        .and_then(ParsedValue::is_string)?;
                    Some(quote!(#key: #str_value))
                }
            }
        });

        let ident = &locale.top_locale_name;
        let pattern = (i != 0).then(|| quote!(#enum_ident::#ident));
        let pattern = pattern.as_ref().unwrap_or(&default_match);
        quote! {
            #pattern => #type_ident {
                #(#filled_string_fields,)*
                #(#init_builder_fields,)*
                #(#subkeys_field_new,)*
            }
        }
    });

    let builder_impls = builders.iter().map(|(_, inter)| &inter.imp);

    let builder_module = builders.is_empty().not().then(move || {
        let empty_type = create_empty_type();
        quote! {
            #[doc(hidden)]
            pub mod builders {
                use super::#enum_ident;

                #empty_type

                #(
                    #builder_impls
                )*
            }
        }
    });

    let (from_locale, const_values) = if !is_namespace {
        let from_locale_match_arms = top_locales
            .iter()
            .map(|locale| quote!(#enum_ident::#locale => &Self::#locale));

        let from_locale = quote! {
            impl leptos_i18n::LocaleKeys for #type_ident {
                type Locale = #enum_ident;
                fn from_locale(_locale: #enum_ident) -> &'static Self {
                    match _locale {
                        #(
                            #from_locale_match_arms,
                        )*
                    }
                }
            }
        };

        let const_values = top_locales
            .iter()
            .map(|locale| quote!(pub const #locale: Self = Self::new(#enum_ident::#locale);));

        let const_values = quote! {
            #(
                #[allow(non_upper_case_globals)]
                #const_values
            )*
        };

        (Some(from_locale), Some(const_values))
    } else {
        (None, None)
    };

    quote! {
        #[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
        #[allow(non_camel_case_types, non_snake_case)]
        pub struct #type_ident {
            #(#string_fields,)*
            #(#builder_fields,)*
            #(#subkeys_fields,)*
        }

        impl #type_ident {

            #const_values

            pub const fn new(_locale: #enum_ident) -> Self {
                match _locale {
                    #(
                        #new_match_arms,
                    )*
                }
            }
        }

        #from_locale

        #builder_module

        #subkeys_module
    }
}

fn create_namespace_mod_ident(namespace_ident: &syn::Ident) -> syn::Ident {
    format_ident!("ns_{}", namespace_ident)
}

fn create_namespaces_types(
    default_locale: &Key,
    keys_ident: &syn::Ident,
    enum_ident: &syn::Ident,
    namespaces: &[Namespace],
    top_locales: &HashSet<&Key>,
    keys: &HashMap<Rc<Key>, BuildersKeysInner>,
) -> TokenStream {
    let namespaces_ts = namespaces.iter().map(|namespace| {
        let namespace_ident = &namespace.key.ident;
        let namespace_module_ident = create_namespace_mod_ident(namespace_ident);
        let keys = keys.get(&namespace.key).unwrap();
        let mut key_path = KeyPath::new(Some(namespace.key.clone()));
        let type_impl = create_locale_type_inner(
            default_locale,
            namespace_ident,
            enum_ident,
            top_locales,
            &namespace.locales,
            &keys.0,
            true,
            &mut key_path,
        );
        quote! {
            pub mod #namespace_module_ident {
                use super::#enum_ident;

                #type_impl
            }
        }
    });

    let namespaces_fields = namespaces.iter().map(|namespace| {
        let key = &namespace.key;
        let namespace_module_ident = create_namespace_mod_ident(&key.ident);
        quote!(pub #key: namespaces::#namespace_module_ident::#key)
    });

    let namespaces_fields_new = namespaces.iter().map(|namespace| {
        let key = &namespace.key;
        let namespace_module_ident = create_namespace_mod_ident(&key.ident);
        quote!(#key: namespaces::#namespace_module_ident::#key::new(_locale))
    });

    let locales = &namespaces.iter().next().unwrap().locales;

    let const_values = locales.iter().map(|locale| {
        let locale_ident = &locale.name;
        quote!(pub const #locale_ident: Self = Self::new(#enum_ident::#locale_ident);)
    });

    let from_locale_match_arms = locales.iter().map(|locale| {
        let locale_ident = &locale.name;
        quote!(#enum_ident::#locale_ident => &Self::#locale_ident)
    });

    quote! {
        pub mod namespaces {
            use super::#enum_ident;

            #(
                #namespaces_ts
            )*

        }

        #[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
        #[allow(non_snake_case)]
        pub struct #keys_ident {
            #(#namespaces_fields,)*
        }

        impl #keys_ident {
            #(
                #[allow(non_upper_case_globals)]
                #const_values
            )*

            pub const fn new(_locale: #enum_ident) -> Self {
                Self {
                    #(
                        #namespaces_fields_new,
                    )*
                }
            }
        }

        impl leptos_i18n::LocaleKeys for #keys_ident {
            type Locale = #enum_ident;
            fn from_locale(_locale: #enum_ident) -> &'static Self {
                match _locale {
                    #(
                        #from_locale_match_arms,
                    )*
                }
            }
        }
    }
}

fn create_locale_type(
    keys: BuildersKeys,
    cfg_file: &ConfigFile,
    keys_ident: &syn::Ident,
    enum_ident: &syn::Ident,
) -> TokenStream {
    let top_locales = cfg_file.locales.iter().map(Deref::deref).collect();
    let default_locale = cfg_file.default.as_ref();
    match keys {
        BuildersKeys::NameSpaces { namespaces, keys } => create_namespaces_types(
            default_locale,
            keys_ident,
            enum_ident,
            namespaces,
            &top_locales,
            &keys,
        ),
        BuildersKeys::Locales { locales, keys } => create_locale_type_inner(
            default_locale,
            keys_ident,
            enum_ident,
            &top_locales,
            locales,
            &keys.0,
            false,
            &mut KeyPath::new(None),
        ),
    }
}
