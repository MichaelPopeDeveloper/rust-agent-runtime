//! Type definitions for the Claude Agent SDK.
//!
//! Everything here is re-exported from the crate root, so `use
//! claude_agent_sdk::*` brings the option, message, permission, hook and MCP
//! types into scope.

/// Define a string-backed enum that round-trips through serde as its wire
/// string and keeps unknown values in an `Other(String)` variant, so newer CLI
/// versions never break deserialization.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $( $(#[$vmeta:meta])* $variant:ident => $s:literal ),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[allow(missing_docs)] // variants are the wire strings, self-describing
        pub enum $name {
            $( $(#[$vmeta])* $variant, )*
            /// A value this SDK version does not know about.
            Other(String),
        }

        impl $name {
            /// The wire representation of this value.
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $s, )*
                    Self::Other(s) => s.as_str(),
                }
            }

            /// Parse a wire string; unknown values become [`Self::Other`].
            pub fn parse(s: &str) -> Self {
                match s {
                    $( $s => Self::$variant, )*
                    other => Self::Other(other.to_string()),
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self::parse(s)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self::parse(&s)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                Ok(Self::parse(&s))
            }
        }
    };
}

mod common;
mod hooks;
mod mcp_config;
mod messages;
mod options;
mod permissions;

pub use common::*;
pub use hooks::*;
pub use mcp_config::*;
pub use messages::*;
pub use options::*;
pub use permissions::*;

/// Boxed, `Send` future used by all callback types.
pub use futures::future::BoxFuture;
