//! Language detection and tree-sitter grammar wiring.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A source language greplm understands for symbol extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Swift,
    Dart,
    /// Recognized as text and indexed, but not parsed for symbols.
    Other,
}

impl Language {
    /// Every language variant, useful for iteration and exhaustive tests.
    pub const ALL: [Language; 15] = [
        Language::Rust,
        Language::Python,
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Go,
        Language::Java,
        Language::C,
        Language::Cpp,
        Language::CSharp,
        Language::Ruby,
        Language::Php,
        Language::Swift,
        Language::Dart,
        Language::Other,
    ];

    /// Short, stable identifier stored in the index and used for `--lang` filters.
    ///
    /// This is the single source of truth for the serialized form; serde
    /// (de)serialization is implemented in terms of `id`/`from_id`.
    pub fn id(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Php => "php",
            Language::Swift => "swift",
            Language::Dart => "dart",
            Language::Other => "other",
        }
    }

    /// Parse a language id back from its stable identifier.
    pub fn from_id(s: &str) -> Option<Language> {
        Some(match s {
            "rust" => Language::Rust,
            "python" => Language::Python,
            "javascript" => Language::JavaScript,
            "typescript" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" => Language::C,
            "cpp" => Language::Cpp,
            "csharp" => Language::CSharp,
            "ruby" => Language::Ruby,
            "php" => Language::Php,
            "swift" => Language::Swift,
            "dart" => Language::Dart,
            "other" => Language::Other,
            _ => return None,
        })
    }

    /// Detect a language from a file extension (without the dot). Matching is
    /// case-insensitive; the common already-lowercased input does not allocate.
    pub fn from_extension(ext: &str) -> Language {
        let lower;
        let ext = if ext.bytes().any(|b| b.is_ascii_uppercase()) {
            lower = ext.to_ascii_lowercase();
            lower.as_str()
        } else {
            ext
        };
        match ext {
            "rs" => Language::Rust,
            "py" | "pyi" | "pyw" => Language::Python,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "tpp" => {
                Language::Cpp
            }
            "cs" => Language::CSharp,
            "rb" | "rake" | "gemspec" => Language::Ruby,
            "php" | "php5" | "php7" | "phtml" => Language::Php,
            "swift" => Language::Swift,
            "dart" => Language::Dart,
            _ => Language::Other,
        }
    }

    /// The tree-sitter grammar for this language, if symbol parsing is supported.
    pub fn grammar(self) -> Option<tree_sitter::Language> {
        let lang = match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Java => tree_sitter_java::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Language::Swift => tree_sitter_swift::LANGUAGE.into(),
            Language::Dart => tree_sitter_dart::LANGUAGE.into(),
            Language::Other => return None,
        };
        Some(lang)
    }
}

impl Serialize for Language {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id())
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <Cow<'de, str>>::deserialize(deserializer)?;
        Language::from_id(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown language id: {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrips_through_from_id() {
        for lang in Language::ALL {
            assert_eq!(Language::from_id(lang.id()), Some(lang));
        }
    }

    #[test]
    fn serde_matches_id() {
        for lang in Language::ALL {
            let json = serde_json::to_string(&lang).unwrap();
            assert_eq!(json, format!("{:?}", lang.id()));
            let back: Language = serde_json::from_str(&json).unwrap();
            assert_eq!(back, lang);
        }
    }

    #[test]
    fn from_extension_is_case_insensitive() {
        assert_eq!(Language::from_extension("RS"), Language::Rust);
        assert_eq!(Language::from_extension("Cpp"), Language::Cpp);
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("dart"), Language::Dart);
        assert_eq!(Language::from_extension("unknownext"), Language::Other);
    }

    #[test]
    fn parseable_languages_have_grammars() {
        for lang in Language::ALL {
            if lang == Language::Other {
                assert!(lang.grammar().is_none());
            } else {
                assert!(lang.grammar().is_some(), "{lang:?} missing grammar");
            }
        }
    }
}
