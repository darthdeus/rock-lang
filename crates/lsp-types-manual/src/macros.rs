/// Generate serde impls for an LSP struct whose fields map to JSON keys.
///
/// The leading direction picks the impls: `out` for what the server only
/// sends (`Serialize`), `in` for what it only reads (`Deserialize`), `both`
/// for both.
///
/// ```ignore
/// impl_lsp_struct!(out TypeName,
///     required { field: Type = "jsonKey", ... },
///     optional { field: Type = "jsonKey", ... }
/// );
/// ```
///
/// `optional` fields are `Option<Type>`: left out of the JSON when `None`, and
/// `None` when the key is absent. Unknown keys are ignored on input.
macro_rules! impl_lsp_struct {
    (both $T:ident, $($fields:tt)*) => {
        impl_lsp_struct!(out $T, $($fields)*);
        impl_lsp_struct!(in $T, $($fields)*);
    };

    (
        out $T:ident,
        required { $( $rf:ident : $rt:ty = $rk:literal ),* $(,)? },
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? }
    ) => {
        #[allow(deprecated)]
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeStruct;
                let hint: usize = 0 $( + { let _: &str = $rk; 1 } )* $( + { let _: &str = $ok; 1 } )*;
                #[allow(unused_mut)]
                let mut st = s.serialize_struct(stringify!($T), hint)?;
                $( st.serialize_field($rk, &self.$rf)?; )*
                $( if let Some(ref v) = self.$of { st.serialize_field($ok, v)?; } )*
                st.end()
            }
        }
    };

    (
        in $T:ident,
        required { $( $rf:ident : $rt:ty = $rk:literal ),* $(,)? },
        optional { $( $of:ident : $ot:ty = $ok:literal ),* $(,)? }
    ) => {
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
                        $( let mut $of: Option<$ot> = None; )*
                        while let Some(key) = map.next_key::<String>()? {
                            match key.as_str() {
                                $( $rk => { $rf = Some(map.next_value()?); } )*
                                $( $ok => { $of = map.next_value()?; } )*
                                _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                            }
                        }
                        Ok($T {
                            $( $rf: $rf.ok_or_else(|| serde::de::Error::missing_field($rk))?, )*
                            $( $of, )*
                        })
                    }
                }
                d.deserialize_map(__Visitor)
            }
        }
    };
}

/// Generate `Serialize` for an untagged enum whose variants each wrap one
/// value: a variant serializes as its inner value.
macro_rules! impl_untagged_enum {
    ($T:ident { $($variant:ident),* $(,)? }) => {
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                match self {
                    $( $T::$variant(v) => v.serialize(s), )*
                }
            }
        }
    };
}
