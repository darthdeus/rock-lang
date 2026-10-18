/// Generate `Serialize` + `Deserialize` impls for a string-keyed unit enum.
///
/// ```ignore
/// impl_str_enum!(MyEnum {
///     Variant = "json-string",
///     ...
/// });
/// ```
macro_rules! impl_str_enum {
    ($T:ident { $($variant:ident = $s:literal),* $(,)? }) => {
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(match self {
                    $( $T::$variant => $s, )*
                })
            }
        }

        impl<'de> serde::Deserialize<'de> for $T {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct __Visitor;
                impl<'de> serde::de::Visitor<'de> for __Visitor {
                    type Value = $T;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        write!(f, concat!("a string for enum ", stringify!($T)))
                    }
                    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<$T, E> {
                        match s {
                            $( $s => Ok($T::$variant), )*
                            _ => Err(E::unknown_variant(s, &[$($s,)*])),
                        }
                    }
                }
                d.deserialize_str(__Visitor)
            }
        }
    };
}

/// Generate `Serialize` + `Deserialize` impls for a tagged enum.
///
/// All variants must be tuple variants with exactly one inner type. The inner
/// type must implement `Serialize` + `Deserialize`.
///
/// ```ignore
/// impl_tagged_enum!(MyEnum, tag = "kind" {
///     Variant(InnerType) = "kindValue",
///     ...
/// });
/// ```
///
/// Serialization merges the inner struct's fields with the tag key.
/// Deserialization collects the map, reads the tag, then delegates to the
/// inner type (which ignores the tag key via the `_` arm in `visit_map`).
macro_rules! impl_tagged_enum {
    ($T:ident, tag = $tag:literal {
        $( $variant:ident($inner:ty) = $kind:literal ),* $(,)?
    }) => {
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let map = match self {
                    $(
                        $T::$variant(inner) => {
                            let mut m = match serde_json::to_value(inner)
                                .map_err(serde::ser::Error::custom)?
                            {
                                serde_json::Value::Object(m) => m,
                                v => {
                                    let mut m = serde_json::Map::new();
                                    m.insert("value".to_string(), v);
                                    m
                                }
                            };
                            m.insert(
                                $tag.to_string(),
                                serde_json::Value::String($kind.to_string()),
                            );
                            m
                        }
                    )*
                };
                serde_json::Value::Object(map).serialize(s)
            }
        }

        impl<'de> serde::Deserialize<'de> for $T {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
                let kind = map
                    .get($tag)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| serde::de::Error::missing_field($tag))?
                    .to_owned();
                let val = serde_json::Value::Object(map);
                match kind.as_str() {
                    $(
                        $kind => serde_json::from_value::<$inner>(val)
                            .map($T::$variant)
                            .map_err(serde::de::Error::custom),
                    )*
                    _ => Err(serde::de::Error::unknown_variant(&kind, &[$($kind,)*])),
                }
            }
        }
    };
}

/// Generate `Serialize` + `Deserialize` impls for an untagged enum.
///
/// All variants must be tuple variants with exactly one inner type.
/// Serialization delegates directly to the inner value.
/// Deserialization buffers through `serde_json::Value` and tries each
/// variant in declaration order, returning the first that succeeds.
///
/// ```ignore
/// impl_untagged_enum!(MyEnum {
///     Variant1(Type1),
///     Variant2(Type2),
/// });
/// ```
macro_rules! impl_untagged_enum {
    ($T:ident { $($variant:ident($vtype:ty)),* $(,)? }) => {
        impl serde::Serialize for $T {
            fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    $( $T::$variant(v) => v.serialize(s), )*
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for $T {
            fn deserialize<D>(d: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let val = serde_json::Value::deserialize(d)?;
                $(
                    if let Ok(v) = serde_json::from_value::<$vtype>(val.clone()) {
                        return Ok($T::$variant(v));
                    }
                )*
                Err(serde::de::Error::custom(concat!(
                    "data did not match any variant of ",
                    stringify!($T)
                )))
            }
        }
    };
}

/// Generate `Serialize` + `Deserialize` impls for a plain LSP struct.
///
/// Syntax:
///
/// ```ignore
/// impl_lsp_struct!(TypeName,
///     required { field: Type = "jsonKey", ... },
///     nullable { field: Type = "jsonKey", ... },
///     optional { field: Type = "jsonKey", ... },
///     flatten { field: Type, ... }
/// );
/// ```
///
/// - `required`: always serialized, must be present when deserializing.
/// - `nullable`: `Option<Type>`, always serialized (`None` → `null`).
///   Corresponds to fields without `#[serde(skip_serializing_if)]`.
/// - `optional`: `Option<Type>`, skipped on serialize when `None`.
///   Corresponds to fields with `#[serde(skip_serializing_if = "Option::is_none")]`.
/// - `flatten`: embedded struct, merged into the JSON object on both sides.
///   Uses a serde_json::Value bridge (one extra allocation per struct with flatten).
///
/// All sections except required accept absent keys as None/default.
macro_rules! impl_lsp_struct {
    // Full form: required + nullable + optional + flatten
    (
        $T:ident,
        required { $( $rf:ident : $rt:ty = $rk:literal ),* $(,)? },
        nullable { $( $nf:ident : $nt:ty = $nk:literal ),* $(,)? },
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? },
        flatten { $( $ff:ident : $ft:ty ),* $(,)? }
    ) => {
        #[allow(deprecated)]
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut map = serde_json::Map::new();
                $(
                    map.insert($rk.to_string(),
                        serde_json::to_value(&self.$rf).map_err(serde::ser::Error::custom)?);
                )*
                $(
                    map.insert($nk.to_string(),
                        serde_json::to_value(&self.$nf).map_err(serde::ser::Error::custom)?);
                )*
                $(
                    if let Some(ref v) = self.$of {
                        map.insert($ok.to_string(),
                            serde_json::to_value(v).map_err(serde::ser::Error::custom)?);
                    }
                )*
                $(
                    if let serde_json::Value::Object(inner) =
                        serde_json::to_value(&self.$ff).map_err(serde::ser::Error::custom)?
                    {
                        map.extend(inner);
                    }
                )*
                serde_json::Value::Object(map).serialize(s)
            }
        }

        #[allow(deprecated)]
        impl<'de> serde::Deserialize<'de> for $T {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
                let val = serde_json::Value::Object(map);
                $(
                    let $rf: $rt = val.get($rk)
                        .ok_or_else(|| serde::de::Error::missing_field($rk))
                        .and_then(|v| serde_json::from_value(v.clone()).map_err(serde::de::Error::custom))?;
                )*
                $(
                    let $nf: Option<$nt> = crate::deserialize_optional(val.get($nk).cloned())?;
                )*
                $(
                    let $of: Option<$ot> = crate::deserialize_optional(val.get($ok).cloned())?;
                )*
                $(
                    let $ff: $ft = serde_json::from_value(val.clone())
                        .map_err(serde::de::Error::custom)?;
                )*
                Ok($T { $($rf,)* $($nf,)* $($of,)* $($ff,)* })
            }
        }
    };

    // No-flatten form: required + nullable + optional (uses fast serialize_struct path)
    (
        $T:ident,
        required { $( $rf:ident : $rt:ty = $rk:literal ),* $(,)? },
        nullable { $( $nf:ident : $nt:ty = $nk:literal ),* $(,)? },
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? }
    ) => {
        #[allow(deprecated)]
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeStruct;
                let hint: usize = 0 $( + { let _: &str = $rk; 1 } )* $( + { let _: &str = $nk; 1 } )* $( + { let _: &str = $ok; 1 } )*;
                #[allow(unused_mut)]
                let mut st = s.serialize_struct(stringify!($T), hint)?;
                $( st.serialize_field($rk, &self.$rf)?; )*
                $( st.serialize_field($nk, &self.$nf)?; )*
                $( if let Some(ref v) = self.$of { st.serialize_field($ok, v)?; } )*
                st.end()
            }
        }

        #[allow(deprecated)]
        impl<'de> serde::Deserialize<'de> for $T {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct __Visitor;
                impl<'de> serde::de::Visitor<'de> for __Visitor {
                    type Value = $T;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        write!(f, concat!("struct ", stringify!($T)))
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<$T, A::Error> {
                        $( let mut $rf: Option<$rt> = None; )*
                        $( let mut $nf: Option<$nt> = None; )*
                        $( let mut $of: Option<$ot> = None; )*
                        while let Some(key) = map.next_key::<String>()? {
                            match key.as_str() {
                                $( $rk => { $rf = Some(map.next_value()?); } )*
                                $( $nk => { $nf = map.next_value()?; } )*
                                $( $ok => { $of = map.next_value()?; } )*
                                _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                            }
                        }
                        Ok($T {
                            $( $rf: $rf.ok_or_else(|| serde::de::Error::missing_field($rk))?, )*
                            $( $nf, )*
                            $( $of, )*
                        })
                    }
                }
                d.deserialize_map(__Visitor)
            }
        }
    };

    // Shorthand: required + optional (no nullable, no flatten)
    (
        $T:ident,
        required { $( $rf:ident : $rt:ty = $rk:literal ),* $(,)? },
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? }
    ) => {
        impl_lsp_struct!($T,
            required { $( $rf: $rt = $rk ),* },
            nullable {},
            optional { $( $of: $ot = $ok ),* }
        );
    };

    // Shorthand: required + optional + flatten (no nullable)
    (
        $T:ident,
        required { $( $rf:ident : $rt:ty = $rk:literal ),* $(,)? },
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? },
        flatten { $( $ff:ident : $ft:ty ),* $(,)? }
    ) => {
        impl_lsp_struct!($T,
            required { $( $rf: $rt = $rk ),* },
            nullable {},
            optional { $( $of: $ot = $ok ),* },
            flatten { $( $ff: $ft ),* }
        );
    };

    // Shorthand: only flatten (no required/nullable/optional)
    (
        $T:ident,
        flatten { $( $ff:ident : $ft:ty ),* $(,)? }
    ) => {
        impl_lsp_struct!($T,
            required {},
            nullable {},
            optional {},
            flatten { $( $ff: $ft ),* }
        );
    };

    // Shorthand: optional + flatten (no required/nullable)
    (
        $T:ident,
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? },
        flatten { $( $ff:ident : $ft:ty ),* $(,)? }
    ) => {
        impl_lsp_struct!($T,
            required {},
            nullable {},
            optional { $( $of: $ot = $ok ),* },
            flatten { $( $ff: $ft ),* }
        );
    };
}
