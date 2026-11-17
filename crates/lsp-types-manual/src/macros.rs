/// Declare an LSP struct and its JSON fields together. The direction is from
/// the server's point of view. Optional fields are omitted when absent.
macro_rules! lsp_struct {
    (
        $direction:ident
        $(#[$attr:meta])*
        $vis:vis struct $T:ident {
            $($(#[$field_attr:meta])* $mode:ident $fv:vis $f:ident: $ty:ty = $key:literal),* $(,)?
        }
    ) => {
        $(#[$attr])*
        $vis struct $T {
            $($(#[$field_attr])* $fv $f: lsp_struct!(@type $mode $ty),)*
        }
        lsp_struct!(@impl $direction $T { $($mode $f: $ty = $key),* });
    };

    (@type required $ty:ty) => { $ty };
    (@type optional $ty:ty) => { Option<$ty> };

    (@impl both $T:ident { $($fields:tt)* }) => {
        lsp_struct!(@impl out $T { $($fields)* });
        lsp_struct!(@impl in $T { $($fields)* });
    };

    (@impl out $T:ident { $($mode:ident $f:ident: $ty:ty = $key:literal),* }) => {
        #[allow(deprecated)]
        impl serde::Serialize for $T {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeStruct;
                let hint: usize = 0 $( + { let _: &str = $key; 1 } )*;
                #[allow(unused_mut)]
                let mut st = s.serialize_struct(stringify!($T), hint)?;
                $(lsp_struct!(@write_required $mode st self $f $key);)*
                $(lsp_struct!(@write_optional $mode st self $f $key);)*
                st.end()
            }
        }
    };

    (@write_required required $st:ident $self:ident $f:ident $key:literal) => {
        $st.serialize_field($key, &$self.$f)?
    };
    (@write_required optional $($unused:tt)*) => {};
    (@write_optional required $($unused:tt)*) => {};
    (@write_optional optional $st:ident $self:ident $f:ident $key:literal) => {
        if let Some(ref value) = $self.$f {
            $st.serialize_field($key, value)?;
        }
    };

    (@impl in $T:ident { $($mode:ident $f:ident: $ty:ty = $key:literal),* }) => {
        impl<'de> serde::Deserialize<'de> for $T {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = $T;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        write!(f, concat!("struct ", stringify!($T)))
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<$T, A::Error> {
                        $(let mut $f: Option<$ty> = None;)*
                        while let Some(key) = map.next_key::<String>()? {
                            match key.as_str() {
                                $($key => $f = lsp_struct!(@read $mode map),)*
                                _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                            }
                        }
                        Ok($T {
                            $($f: lsp_struct!(@finish $mode $f $key),)*
                        })
                    }
                }
                d.deserialize_map(Visitor)
            }
        }
    };

    (@read required $map:ident) => { Some($map.next_value()?) };
    (@read optional $map:ident) => { $map.next_value()? };
    (@finish required $f:ident $key:literal) => { $f.ok_or_else(|| serde::de::Error::missing_field($key))? };
    (@finish optional $f:ident $key:literal) => { $f };
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
