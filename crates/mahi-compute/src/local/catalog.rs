//! Curated catalog of good local models as single-file Q4_K_M GGUF downloads.
//!
//! Every entry is a direct Hugging Face `resolve/main` URL so the
//! [`crate::local::ModelManager`] can stream it without any HF API calls.
//! Qwen models come from the official `Qwen/...-GGUF` repos where they ship a
//! single file; everything else uses the well-maintained `bartowski` quants
//! (the official Qwen 7B repos split Q4_K_M across multiple files, which we
//! deliberately avoid).
//!
//! `size_bytes` values are the sizes published on the Hugging Face file
//! listings (base-10). They are advisory: the downloader always prefers the
//! `Content-Length`/`Content-Range` reported by the server at download time.

use mahi_contracts::types::{
    CapabilitySet, LimitationLabel, ModelDescriptor, ModelSource, PerfProfile,
};
use serde::{Deserialize, Serialize};

/// One downloadable model in the curated local catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Stable id; the model is stored on disk as `<id>.gguf`.
    pub id: String,
    pub display_name: String,
    /// Model family, e.g. `"qwen2.5"` or `"llama-3.2"`.
    pub family: String,
    /// Published download size in bytes (advisory; server values win).
    pub size_bytes: u64,
    /// Quantization label, e.g. `"Q4_K_M"`.
    pub quantization: String,
    /// Context window in tokens (as shipped in the GGUF metadata).
    pub context_window: u32,
    /// Whether the model reliably emits OpenAI-style tool calls under
    /// llama-server's `--jinja` chat templates.
    pub tool_calling: bool,
    /// Direct single-file download URL (`https://huggingface.co/<repo>/resolve/main/<file>`).
    pub download_url: String,
    /// The file name component of `download_url`.
    pub file_name: String,
    pub description: String,
}

/// Build a Hugging Face `resolve/main` URL for `repo`/`file`.
fn hf_url(repo: &str, file: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/main/{file}")
}

#[allow(clippy::too_many_arguments)]
fn entry(
    id: &str,
    display_name: &str,
    family: &str,
    size_bytes: u64,
    context_window: u32,
    tool_calling: bool,
    repo: &str,
    file_name: &str,
    description: &str,
) -> CatalogEntry {
    CatalogEntry {
        id: id.to_string(),
        display_name: display_name.to_string(),
        family: family.to_string(),
        size_bytes,
        quantization: "Q4_K_M".to_string(),
        context_window,
        tool_calling,
        download_url: hf_url(repo, file_name),
        file_name: file_name.to_string(),
        description: description.to_string(),
    }
}

/// The curated list of local models the app offers for download.
pub fn model_catalog() -> Vec<CatalogEntry> {
    vec![
        entry(
            "qwen2.5-0.5b-instruct",
            "Qwen2.5 0.5B Instruct",
            "qwen2.5",
            398_000_000,
            32_768,
            true,
            "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
            "qwen2.5-0.5b-instruct-q4_k_m.gguf",
            "Tiny and fast; instant responses on any Apple Silicon Mac. \
             Good for quick drafts and testing, limited reasoning depth.",
        ),
        entry(
            "llama-3.2-1b-instruct",
            "Llama 3.2 1B Instruct",
            "llama-3.2",
            808_000_000,
            131_072,
            true,
            "bartowski/Llama-3.2-1B-Instruct-GGUF",
            "Llama-3.2-1B-Instruct-Q4_K_M.gguf",
            "Meta's smallest Llama 3.2; very fast with a long context. \
             Good for summarization and chat on modest hardware.",
        ),
        entry(
            "llama-3.2-3b-instruct",
            "Llama 3.2 3B Instruct",
            "llama-3.2",
            2_020_000_000,
            131_072,
            true,
            "bartowski/Llama-3.2-3B-Instruct-GGUF",
            "Llama-3.2-3B-Instruct-Q4_K_M.gguf",
            "Strong small model: a good speed/quality default for 8 GB Macs, \
             with solid instruction following and tool use.",
        ),
        entry(
            "llama-3.1-8b-instruct",
            "Llama 3.1 8B Instruct",
            "llama-3.1",
            4_920_000_000,
            131_072,
            true,
            "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF",
            "Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
            "The workhorse 8B: strong general assistant with reliable tool \
             calling and a 128K context. Wants 16 GB of unified memory.",
        ),
        entry(
            "qwen2.5-7b-instruct",
            "Qwen2.5 7B Instruct",
            "qwen2.5",
            4_680_000_000,
            32_768,
            true,
            "bartowski/Qwen2.5-7B-Instruct-GGUF",
            "Qwen2.5-7B-Instruct-Q4_K_M.gguf",
            "Excellent all-round 7B; top-tier instruction following, math, \
             and multilingual chat for its size.",
        ),
        entry(
            "qwen2.5-coder-7b-instruct",
            "Qwen2.5 Coder 7B Instruct",
            "qwen2.5-coder",
            4_680_000_000,
            32_768,
            true,
            "bartowski/Qwen2.5-Coder-7B-Instruct-GGUF",
            "Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf",
            "Code-specialized Qwen2.5: the best local coding model at this \
             size. Great for code generation, repair, and explanation.",
        ),
        entry(
            "mistral-7b-instruct-v0.3",
            "Mistral 7B Instruct v0.3",
            "mistral",
            4_370_000_000,
            32_768,
            true,
            "bartowski/Mistral-7B-Instruct-v0.3-GGUF",
            "Mistral-7B-Instruct-v0.3-Q4_K_M.gguf",
            "Classic fast 7B with native function-calling support and a \
             concise, direct style.",
        ),
        entry(
            "phi-3.5-mini-instruct",
            "Phi-3.5 Mini Instruct",
            "phi-3.5",
            2_390_000_000,
            131_072,
            false,
            "bartowski/Phi-3.5-mini-instruct-GGUF",
            "Phi-3.5-mini-instruct-Q4_K_M.gguf",
            "Microsoft's 3.8B reasoning-dense small model with a 128K \
             context. Strong for its size; no reliable tool calling.",
        ),
        entry(
            "gemma-2-9b-it",
            "Gemma 2 9B IT",
            "gemma-2",
            5_760_000_000,
            8_192,
            false,
            "bartowski/gemma-2-9b-it-GGUF",
            "gemma-2-9b-it-Q4_K_M.gguf",
            "Google's Gemma 2 9B: great prose and chat quality. Short 8K \
             context and no tool calling.",
        ),
    ]
}

/// Look up a catalog entry by its stable id.
pub fn find_entry(id: &str) -> Option<CatalogEntry> {
    model_catalog().into_iter().find(|e| e.id == id)
}

/// Map a catalog entry to the contracts' [`ModelDescriptor`]
/// (source = [`ModelSource::OnDevice`]).
pub fn catalog_descriptor(entry: &CatalogEntry) -> ModelDescriptor {
    let mut limitations = vec![LimitationLabel::MaxContextWindow(entry.context_window)];
    if !entry.tool_calling {
        limitations.push(LimitationLabel::Custom("No tool calling".to_string()));
    }
    ModelDescriptor {
        id: entry.id.clone(),
        display_name: entry.display_name.clone(),
        context_window: entry.context_window,
        capabilities: CapabilitySet {
            vision: false,
            tool_calling: entry.tool_calling,
            // What the model *offers*: callers' `required_caps.min_context_window`
            // is checked against this via `CapabilitySet::satisfied_by`.
            min_context_window: entry.context_window,
            code_gen: true,
            // The local catalog doesn't track a per-model reasoning toggle yet;
            // thinking is a soft request hint, so leaving this false is safe.
            thinking: false,
        },
        limitations,
        size_bytes: Some(entry.size_bytes),
        quantization: Some(entry.quantization.clone()),
        source: ModelSource::OnDevice,
        perf_profile: PerfProfile::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_ids_are_unique() {
        let catalog = model_catalog();
        let ids: HashSet<_> = catalog.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), catalog.len(), "duplicate catalog ids");
    }

    #[test]
    fn catalog_contains_required_models() {
        let catalog = model_catalog();
        for id in [
            "qwen2.5-0.5b-instruct",
            "llama-3.2-1b-instruct",
            "llama-3.2-3b-instruct",
            "llama-3.1-8b-instruct",
            "qwen2.5-7b-instruct",
            "qwen2.5-coder-7b-instruct",
            "mistral-7b-instruct-v0.3",
            "phi-3.5-mini-instruct",
            "gemma-2-9b-it",
        ] {
            assert!(
                catalog.iter().any(|e| e.id == id),
                "missing required model {id}"
            );
        }
    }

    #[test]
    fn catalog_entries_are_well_formed() {
        for e in model_catalog() {
            assert!(!e.id.is_empty());
            assert!(!e.display_name.is_empty());
            assert!(!e.family.is_empty());
            assert!(!e.description.is_empty());
            assert!(e.size_bytes > 0, "{}: size must be > 0", e.id);
            assert!(e.context_window >= 4096, "{}: implausible context", e.id);
            assert_eq!(e.quantization, "Q4_K_M");
            assert!(
                e.download_url.starts_with("https://huggingface.co/"),
                "{}: not an HF URL: {}",
                e.id,
                e.download_url
            );
            assert!(
                e.download_url.contains("/resolve/main/"),
                "{}: not a resolve/main URL",
                e.id
            );
            assert!(
                e.download_url.ends_with(&e.file_name),
                "{}: url/file_name mismatch",
                e.id
            );
            assert!(
                e.file_name.to_ascii_lowercase().ends_with(".gguf"),
                "{}: not a .gguf file",
                e.id
            );
            // Single-file downloads only: split GGUFs are named -00001-of-000NN.
            assert!(
                !e.file_name.contains("-of-"),
                "{}: split GGUF not allowed",
                e.id
            );
            // Ids must be safe as `<id>.gguf` file stems and round-trip
            // through `Path::file_stem` (no path separators; a final
            // `.gguf` extension is the only one stripped).
            assert!(!e.id.contains('/') && !e.id.contains('\\'));
        }
    }

    #[test]
    fn tool_calling_flags_match_model_families() {
        for e in model_catalog() {
            let expected = match e.family.as_str() {
                "qwen2.5" | "qwen2.5-coder" | "llama-3.2" | "llama-3.1" | "mistral" => true,
                "phi-3.5" | "gemma-2" => false,
                other => panic!("unexpected family {other}"),
            };
            assert_eq!(e.tool_calling, expected, "{}", e.id);
        }
    }

    #[test]
    fn find_entry_matches_catalog() {
        let entry = find_entry("qwen2.5-0.5b-instruct").expect("known id");
        assert_eq!(entry.display_name, "Qwen2.5 0.5B Instruct");
        assert!(find_entry("not-a-model").is_none());
    }

    #[test]
    fn descriptor_mapping_is_faithful() {
        let entry = find_entry("llama-3.1-8b-instruct").unwrap();
        let desc = catalog_descriptor(&entry);
        assert_eq!(desc.id, entry.id);
        assert_eq!(desc.display_name, entry.display_name);
        assert_eq!(desc.context_window, entry.context_window);
        assert!(desc.capabilities.tool_calling);
        assert!(!desc.capabilities.vision);
        assert_eq!(desc.capabilities.min_context_window, entry.context_window);
        assert_eq!(desc.size_bytes, Some(entry.size_bytes));
        assert_eq!(desc.quantization.as_deref(), Some("Q4_K_M"));
        assert_eq!(desc.source, ModelSource::OnDevice);
        assert!(desc
            .limitations
            .contains(&LimitationLabel::MaxContextWindow(entry.context_window)));
    }

    #[test]
    fn descriptor_flags_missing_tool_calling() {
        let entry = find_entry("gemma-2-9b-it").unwrap();
        let desc = catalog_descriptor(&entry);
        assert!(!desc.capabilities.tool_calling);
        assert!(desc
            .limitations
            .iter()
            .any(|l| matches!(l, LimitationLabel::Custom(msg) if msg.contains("tool"))));
    }
}
