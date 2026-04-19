use anyhow::Result;
use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::fs::File;
use tokio::io::{stdin, stdout};
use tokio::process::Command as ProcessCommand;
use tokio::sync::Semaphore;
use tokio::time::{Duration, timeout};
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use url::Url;
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize, Clone)]
struct InitializationOptions {
    rulesets: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct MagoSettings {
    rulesets: Option<String>,
}

#[derive(Debug, Clone)]
struct CompressedDocument {
    compressed_data: Vec<u8>,
    original_size: usize,
    checksum: String,
    compression_ratio: f32,
}

#[derive(Debug, Clone)]
struct CachedResults {
    diagnostics: Vec<Diagnostic>,
    result_id: String,
    generated_at: Instant,
    content_checksum: String, // Track content version to detect changes
}

#[derive(Debug, Clone)]
struct MagoLanguageServer {
    client: Client,
    // Compressed document storage to reduce memory usage
    open_docs: std::sync::Arc<std::sync::RwLock<HashMap<Url, CompressedDocument>>>,
    // Cache Mago results to avoid redundant analysis
    results_cache: std::sync::Arc<std::sync::RwLock<HashMap<Url, CachedResults>>>,
    // Memory tracking
    total_memory_usage: std::sync::Arc<AtomicUsize>,
    rulesets: std::sync::Arc<std::sync::RwLock<Option<String>>>, // None means use Mago defaults
    mago_path: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    workspace_root: std::sync::Arc<std::sync::RwLock<Option<std::path::PathBuf>>>,
    // Limit concurrent Mago processes to prevent system overload
    process_semaphore: std::sync::Arc<Semaphore>,
}

impl MagoLanguageServer {
    fn new(client: Client) -> Self {
        Self {
            client,
            open_docs: std::sync::Arc::new(std::sync::RwLock::new(HashMap::with_capacity(100))),
            results_cache: std::sync::Arc::new(std::sync::RwLock::new(HashMap::with_capacity(100))),
            total_memory_usage: std::sync::Arc::new(AtomicUsize::new(0)),
            rulesets: std::sync::Arc::new(std::sync::RwLock::new(None)), // Let Mago use its defaults
            mago_path: std::sync::Arc::new(std::sync::RwLock::new(None)),
            workspace_root: std::sync::Arc::new(std::sync::RwLock::new(None)),
            // Limit to 4 concurrent Mago processes to avoid overwhelming the system
            process_semaphore: std::sync::Arc::new(Semaphore::new(4)),
        }
    }

    fn compress_document(&self, content: &str) -> CompressedDocument {
        let start = Instant::now();
        let original_size = content.len();

        // Use LZ4 for fast compression
        let compressed_data = compress_prepend_size(content.as_bytes());
        let compressed_size = compressed_data.len();
        let compression_ratio = compressed_size as f32 / original_size as f32;

        // Compute checksum for cache invalidation
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let checksum = format!("{:x}", hasher.finalize());

        let elapsed = start.elapsed();
        eprintln!(
            "📦 Mago LSP: Compressed in {:.2}ms: {}KB → {}KB ({:.1}% ratio)",
            elapsed.as_secs_f64() * 1000.0,
            original_size / 1024,
            compressed_size / 1024,
            compression_ratio * 100.0
        );

        // Update memory tracking
        self.total_memory_usage
            .fetch_add(compressed_size, Ordering::Relaxed);

        CompressedDocument {
            compressed_data,
            original_size,
            checksum,
            compression_ratio,
        }
    }

    fn decompress_document(&self, doc: &CompressedDocument) -> Result<String> {
        let start = Instant::now();
        let decompressed = decompress_size_prepended(&doc.compressed_data)
            .map_err(|e| anyhow::anyhow!("Decompression failed: {}", e))?;

        let content = String::from_utf8(decompressed)
            .map_err(|e| anyhow::anyhow!("UTF-8 conversion failed: {}", e))?;

        let elapsed = start.elapsed();
        if elapsed.as_millis() > 5 {
            eprintln!(
                "⚠️ Mago LSP: Slow decompression: {:.2}ms for {}KB",
                elapsed.as_secs_f64() * 1000.0,
                doc.original_size / 1024
            );
        }

        Ok(content)
    }

    fn get_memory_usage_mb(&self) -> f32 {
        self.total_memory_usage.load(Ordering::Relaxed) as f32 / 1_048_576.0
    }

    fn log_memory_stats(&self) {
        if let Ok(docs) = self.open_docs.read() {
            let doc_count = docs.len();
            let total_original: usize = docs.values().map(|d| d.original_size).sum();
            let total_compressed: usize = docs.values().map(|d| d.compressed_data.len()).sum();
            let avg_ratio = if doc_count > 0 {
                docs.values().map(|d| d.compression_ratio).sum::<f32>() / doc_count as f32
            } else {
                0.0
            };

            eprintln!("📊 Mago LSP Memory Stats:");
            eprintln!("  📁 Documents: {}", doc_count);
            eprintln!(
                "  💾 Compressed: {:.1}MB (from {:.1}MB original)",
                total_compressed as f32 / 1_048_576.0,
                total_original as f32 / 1_048_576.0
            );
            eprintln!("  📉 Average compression: {:.1}%", avg_ratio * 100.0);
            eprintln!(
                "  🗄️ Results cached: {}",
                self.results_cache.read().map(|c| c.len()).unwrap_or(0)
            );
        }
    }

    fn get_mago_path(&self) -> String {
        // First check the cache
        if let Ok(guard) = self.mago_path.read() {
            if let Some(cached_path) = &*guard {
                eprintln!("📂 Mago LSP: Using cached Mago path: {}", cached_path);
                return cached_path.clone();
            }
        }

        //eprintln!("🔍 Mago LSP: Detecting Mago path...");
        eprintln!("🔄 Mago LSP: Using system mago");
        let mago_path = "mago".to_string();

        eprintln!("🎯 Mago LSP: Final Mago path: {}", mago_path);

        // Cache the result
        if let Ok(mut guard) = self.mago_path.write() {
            *guard = Some(mago_path.clone());
        }

        mago_path
    }

    fn discover_rulesets(&self, workspace_root: Option<&std::path::Path>) {
        eprintln!("🔍 Mago LSP: Discovering Mago configuration files...");

        if let Some(root) = workspace_root {
            let config_files = ["mago.toml", "mago.yaml", "mago.json"];

            for config_file in &config_files {
                let config_path = root.join(config_file);

                if config_path.exists() {
                    if let Some(path_str) = config_path.to_str() {
                        eprintln!("✅ Mago LSP: Using config file: {}", config_path.display());
                        if let Ok(mut rulesets_guard) = self.rulesets.write() {
                            // Store the full path to the config file
                            *rulesets_guard = Some(path_str.to_string());
                        }
                        return;
                    } else {
                        eprintln!("⚠️ Mago LSP: Could not read config file: {}", config_file);
                    }
                }
            }
            eprintln!("🔍 Mago LSP: Mago config files found in project root");
        }
    }

    fn find_project_root(&self, uri: &Url) -> std::path::PathBuf {
        if let Ok(file_path) = uri.to_file_path() {
            let mut current = file_path.parent();

            while let Some(dir) = current {
                // Check for project markers (in order of likelihood)
                if dir.join("composer.json").exists()
                    || dir.join("mago.toml").exists()
                    || dir.join(".git").exists()
                {
                    eprintln!("🎯 Mago LSP: Found project root at: {}", dir.display());
                    return dir.to_path_buf();
                }
                current = dir.parent();
            }
        }

        // Fallback to workspace root or current directory
        let fallback = self
            .workspace_root
            .read()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        eprintln!(
            "⚠️ Mago LSP: No project markers found, using fallback: {}",
            fallback.display()
        );
        fallback
    }

    async fn run_mago(
        &self,
        uri: &Url,
        file_path: &str,
        command: &str,
        temp_file_name: &str,
        temp_file_path: &PathBuf,
    ) -> Result<Vec<Diagnostic>> {
        let start_time = Instant::now();
        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        eprintln!(
            "🔍 Mago LSP: Starting {} for file: {} (URI: {})",
            command, file_name, uri
        );

        // Acquire semaphore permit to limit concurrent Mago processes
        let available_permits = self.process_semaphore.available_permits();
        let _permit = self
            .process_semaphore
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to acquire process semaphore: {}", e))?;
        eprintln!(
            "🎫 Mago LSP: Acquired process slot for {} (slots in use: {}/4)",
            file_name,
            4 - available_permits
        );

        // Use cached Mago path
        let mago_path = self.get_mago_path();

        // Find the project root for this specific file
        let project_root = self.find_project_root(uri);
        eprintln!(
            "📁 Mago LSP: Using project root: {}",
            project_root.display()
        );

        // Check if we need to discover config files (if none set or using fallback)
        let should_discover = if let Ok(rulesets_guard) = self.rulesets.read() {
            match &*rulesets_guard {
                None => true,
                _ => false,
            }
        } else {
            false
        };

        if should_discover {
            eprintln!("🔍 Mago LSP: Checking for config files in project root...");
            self.discover_rulesets(Some(&project_root));
        }

        eprintln!("⚙️ Mago LSP: Using direct execution for: {}", mago_path);
        let mut cmd = ProcessCommand::new(&mago_path);

        eprintln!("🚀 Mago LSP: Running Mago on {}", file_name);

        let file = File::open(&temp_file_path).await?.into_std().await;

        // Add config file path after the file path and format
        if let Ok(rulesets_guard) = self.rulesets.read() {
            if let Some(ref rulesets) = *rulesets_guard {
                // Check if this is a path to a config file or ruleset names
                if rulesets.ends_with(".toml")
                    || rulesets.ends_with(".yaml")
                    || rulesets.ends_with(".json")
                {
                    eprintln!("📋 Mago LSP: Using config file: {}", rulesets);
                    cmd.args(&["--config", rulesets]);
                }
            } else {
                eprintln!("📋 Mago LSP: Using all default rulesets");
            }
        }

        // Add Mago arguments
        cmd.arg(&command)
            .args(&["--reporting-format", "json"]) // Use JSON output format
            .args(&["--stdin-input", &file_path]) // Use STDIN
            .stdin(std::process::Stdio::from(file))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true); // Ensure process is killed if dropped

        eprintln!("🔍 Mago LSP: Running Mago on temp file: {}", temp_file_name);

        let child = match cmd.spawn() {
            Ok(child) => {
                eprintln!("✅ Mago LSP: Successfully spawned Mago process");
                child
            }
            Err(e) => {
                eprintln!("❌ Mago LSP: Failed to spawn Mago for {}: {}", file_name, e);
                // Clean up temp file on error
                let _ = std::fs::remove_file(&temp_file_path);
                return Err(anyhow::anyhow!("Mago error: {}", e));
            }
        };

        // Wait for output with timeout (10 seconds for Mago execution)
        let output = match timeout(Duration::from_secs(10), child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let elapsed = start_time.elapsed();
                eprintln!(
                    "⚡ Mago LSP: Process completed for {} in {:.2}s",
                    file_name,
                    elapsed.as_secs_f64()
                );
                output
            }
            Ok(Err(e)) => {
                let elapsed = start_time.elapsed();
                eprintln!(
                    "❌ Mago LSP: Mago process error for {} after {:.2}s: {}",
                    file_name,
                    elapsed.as_secs_f64(),
                    e
                );
                return Err(anyhow::anyhow!(
                    "Mago process error for {}: {}",
                    file_name,
                    e
                ));
            }
            Err(_) => {
                eprintln!("⏱️ Mago LSP: Mago timeout for {} (>10s)", file_name);
                // Process will be killed automatically due to kill_on_drop(true)
                return Err(anyhow::anyhow!(
                    "Mago execution timeout for {} after 10 seconds",
                    file_name
                ));
            }
        };

        let raw_output = String::from_utf8_lossy(&output.stdout);

        // Debug: Show raw Mago output (first 500 chars)
        let output_preview = if raw_output.len() > 500 {
            format!("{}...", &raw_output[..500])
        } else {
            raw_output.to_string()
        };
        eprintln!(
            "🔬 Mago LSP: Raw Mago output for {}: {}",
            file_name, output_preview
        );

        // Permit is automatically released when it goes out of scope
        drop(_permit);
        let available_after = self.process_semaphore.available_permits();
        eprintln!(
            "🎫 Mago LSP: Released process slot for {} (slots available: {}/4)",
            file_name, available_after
        );

        // Extract JSON from raw output (Mago might output debug info before JSON)
        let json_output = self.extract_json_from_output(&raw_output);
        let diagnostics = self.parse_mago_output(&json_output, uri).await?;

        // Log results with timing
        let total_time = start_time.elapsed();
        let issue_count = diagnostics.len();
        if issue_count == 0 {
            eprintln!(
                "✅ Mago LSP: {} is clean! No issues found (took {:.2}s)",
                file_name,
                total_time.as_secs_f64()
            );
        } else {
            let errors = diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();
            let warnings = diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
                .count();
            let infos = diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::INFORMATION))
                .count();

            eprintln!(
                "📊 Mago LSP: {} {} issues found in {}: {} errors, {} warnings, {} info (took {:.2}s)",
                command,
                issue_count,
                file_name,
                errors,
                warnings,
                infos,
                total_time.as_secs_f64()
            );
        }

        Ok(diagnostics)
    }

    fn extract_json_from_output(&self, output: &str) -> String {
        // Mago might output debug information before the JSON
        // Find the first '{' and last '}' to extract the JSON object

        if let Some(start) = output.find('{') {
            // Find the matching closing brace by counting braces
            let mut brace_count = 0;
            let mut in_string = false;
            let mut escape_next = false;
            let bytes = output.as_bytes();

            for i in start..bytes.len() {
                let ch = bytes[i] as char;

                if escape_next {
                    escape_next = false;
                    continue;
                }

                if ch == '\\' && in_string {
                    escape_next = true;
                    continue;
                }

                if ch == '"' && !in_string {
                    in_string = true;
                } else if ch == '"' && in_string {
                    in_string = false;
                }

                if !in_string {
                    if ch == '{' {
                        brace_count += 1;
                    } else if ch == '}' {
                        brace_count -= 1;
                        if brace_count == 0 {
                            // Found the matching closing brace
                            let json_str = &output[start..=i];
                            eprintln!(
                                "📋 Mago LSP: Extracted JSON from position {} to {}",
                                start, i
                            );
                            return json_str.to_string();
                        }
                    }
                }
            }
        }

        // If no valid JSON found, return the original output
        eprintln!("⚠️ Mago LSP: Could not extract JSON from output, using raw output");
        output.to_string()
    }

    async fn parse_mago_output(&self, json_output: &str, uri: &Url) -> Result<Vec<Diagnostic>> {
        // Early return if empty output
        if json_output.trim().is_empty() {
            return Ok(vec![]);
        }

        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        // Debug: Parse and show violations
        eprintln!(
            "🔬 Mago LSP: Parsing {} bytes of JSON output for {}",
            json_output.len(),
            file_name
        );

        let mut diagnostics = Vec::with_capacity(10); // Pre-allocate for common case

        // Parse Mago JSON output
        let mago_result: serde_json::Value = match serde_json::from_str(json_output) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("❌ Mago LSP: Failed to parse JSON output: {}", e);
                eprintln!("Raw output: {}", json_output);
                return Ok(vec![]);
            }
        };

        // Mago JSON structure has "issues" array
        if let Some(issues) = mago_result.get("issues").and_then(|f| f.as_array()) {
            eprintln!(
                "📁 Mago LSP: Found {} issue(s) in Mago output",
                issues.len()
            );

            eprintln!("🔍 Mago LSP: {} issues", issues.len());

            for (issue_idx, issue_entry) in issues.iter().enumerate() {
                if let Some(diagnostic) = self.convert_issue_to_diagnostic(issue_entry, uri).await {
                    diagnostics.push(diagnostic);
                    eprintln!(
                        "✅ Mago LSP: Successfully converted issue #{} to diagnostic for {}",
                        issue_idx + 1,
                        file_name
                    );
                } else {
                    eprintln!(
                        "⚠️ Mago LSP: Failed to convert issue #{} to diagnostic for {}",
                        issue_idx + 1,
                        file_name
                    );
                }
            }
        } else {
            eprintln!("⚠️ Mago LSP: No 'issue' array found in Mago output");
        }

        eprintln!(
            "📊 Mago LSP: Total diagnostics generated for {}: {}",
            file_name,
            diagnostics.len()
        );
        Ok(diagnostics)
    }

    async fn convert_issue_to_diagnostic(
        &self,
        issue: &serde_json::Value,
        uri: &Url,
    ) -> Option<Diagnostic> {
        // Mago JSON issue structure:
        // {
        //   "level": "Warning",
        //   "code": "strict-types",
        //   "message": "Missing xxx of the file.",
        //   "notes": [
        //     "The xxx hoge fuga."
        //   ],
        //   "help": "Add xxx at top of youf file.",
        //   "annotations": [
        //     {
        //       "kind": "Primary",
        //       "span": {
        //         "file_id": {
        //           "name": "src/xxx.php",
        //           "path": "/path/to/src/xxx.php",
        //           "size": 341,
        //           "file_type": "Host"
        //         },
        //         "start": {
        //           "offset": 0,
        //           "line": 0
        //         },
        //         "end": {
        //           "offset": 5,
        //           "line": 0
        //         }
        //       }
        //     }
        //   ]
        // }

        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        eprintln!(
            "🎯 Mago LSP: Converting issue to diagnostic for URI: {}",
            file_name
        );

        // With temp file approach, each analysis is isolated so no validation needed
        let level = issue["level"].as_str().unwrap_or("");
        let code = issue["code"].as_str().unwrap_or("");
        let message = issue["message"].as_str().unwrap_or("");
        // let help = issue["help"].as_str().unwrap_or("");
        // let note = issue["notes"][0].as_str().unwrap_or("");

        let annotation = &issue["annotations"][0];
        // let kind = annotation["kind"].as_str().unwrap_or("");
        let start_offset = annotation["span"]["start"]["offset"].as_u64()? as u32;
        let start_line = annotation["span"]["start"]["line"].as_u64()? as u32;
        let end_offset = annotation["span"]["end"]["offset"].as_u64()? as u32;
        let end_line = annotation["span"]["end"]["line"].as_u64()? as u32;

        let severity = match level {
            "Error" => DiagnosticSeverity::ERROR,
            "Warning" => DiagnosticSeverity::WARNING,
            "Help" => DiagnosticSeverity::HINT,
            _ => DiagnosticSeverity::INFORMATION,
        };

        // Create range with proper boundaries
        let range = Range {
            start: Position {
                line: start_line,
                character: start_offset,
            },
            end: Position {
                line: end_line,
                character: end_offset,
            },
        };

        Some(Diagnostic {
            range,
            severity: Some(severity),
            code: if !code.is_empty() {
                Some(NumberOrString::String(code.to_string()))
            } else {
                None
            },
            source: Some("mago".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            code_description: None,
            data: None,
        })
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for MagoLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        eprintln!("🚀 Mago LSP: Server initializing...");
        eprintln!("🔧 Mago LSP: Client info: {:?}", params.client_info);

        // Determine workspace root for config file lookup
        let workspace_root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok());

        if let Some(ref root) = workspace_root {
            eprintln!("📁 Mago LSP: Workspace root: {}", root.display());
        } else {
            eprintln!("❌ Mago LSP: No workspace root detected");
        }

        // Store workspace root for Mago path detection
        if let Ok(mut workspace_guard) = self.workspace_root.write() {
            *workspace_guard = workspace_root.clone();
        }

        let mut should_discover = true;

        if let Some(options) = params.initialization_options {
            // Parse initialization options
            eprintln!("📦 Mago LSP: Processing initialization options from extension");
            match serde_json::from_value::<InitializationOptions>(options.clone()) {
                Ok(init_options) => {
                    if let Some(rulesets) = init_options.rulesets {
                        eprintln!("⚙️ Mago LSP: Extension provided rulesets: '{}'", rulesets);
                        if let Ok(mut rulesets_guard) = self.rulesets.write() {
                            *rulesets_guard = Some(rulesets.clone());
                        }
                        should_discover = false; // Don't discover if rulesets were explicitly provided
                    } else {
                        eprintln!(
                            "🎯 Mago LSP: No rulesets provided by extension - will discover from workspace"
                        );
                    }
                }
                Err(e) => {
                    eprintln!("❌ Mago LSP: Failed to parse initialization options: {}", e);
                }
            }
        } else {
            eprintln!(
                "📋 Mago LSP: No initialization options provided - will discover from workspace"
            );
        }

        // Discover from workspace if no explicit rulesets were provided
        if should_discover {
            self.discover_rulesets(workspace_root.as_deref());
        }

        // Log final initialization state
        if let Ok(rulesets_guard) = self.rulesets.read() {
            match &*rulesets_guard {
                Some(rulesets) => {
                    if rulesets.ends_with(".toml")
                        || rulesets.ends_with(".yaml")
                        || rulesets.ends_with(".json")
                    {
                        eprintln!("🎯 Mago LSP: Initialized with config file: '{}'", rulesets);
                        eprintln!("📋 Mago LSP: Configuration source: Project-specific ruleset");
                    } else {
                        eprintln!("🎯 Mago LSP: Initialized with rulesets: '{}'", rulesets);
                        eprintln!(
                            "📋 Mago LSP: Configuration source: Custom ruleset configuration"
                        );
                    }
                }
                None => {
                    eprintln!("🎯 Mago LSP: Initialized with default rulesets");
                    eprintln!("📋 Mago LSP: Configuration source: Built-in defaults");
                }
            }
        }

        eprintln!("✅ Mago LSP: Server initialization complete!");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("mago".to_string()),
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        ..Default::default()
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        eprintln!("🎉 Mago LSP: Server is ready and operational!");
        // Pre-cache the Mago path on initialization
        let _ = self.get_mago_path();
        eprintln!("🚀 Mago LSP: Ready to analyze PHP files!");
    }

    async fn shutdown(&self) -> LspResult<()> {
        eprintln!("🔄 Mago LSP: Shutting down, clearing caches...");

        // Clear all cached data on shutdown
        if let Ok(mut docs) = self.open_docs.write() {
            docs.clear();
        }
        if let Ok(mut cache) = self.results_cache.write() {
            cache.clear();
        }

        // Reset memory counter
        self.total_memory_usage.store(0, Ordering::Relaxed);

        eprintln!("✅ Mago LSP: Shutdown complete");
        Ok(())
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Clear document from memory to prevent memory leaks
        let uri = params.text_document.uri;

        // Remove compressed document and update memory tracking
        if let Ok(mut docs) = self.open_docs.write() {
            if let Some(doc) = docs.remove(&uri) {
                let freed_memory = doc.compressed_data.len();
                self.total_memory_usage
                    .fetch_sub(freed_memory, Ordering::Relaxed);
                eprintln!(
                    "🗑️ Mago LSP: Closed file, freed {}KB, total memory: {:.1}MB",
                    freed_memory / 1024,
                    self.get_memory_usage_mb()
                );
            }
        }

        // Clear cached results
        if let Ok(mut cache) = self.results_cache.write() {
            let removed = cache.remove(&uri);
            eprintln!(
                "🗑️ Mago LSP: Cache cleared on close for URI: {} - removed: {}",
                uri,
                removed.is_some()
            );
        }

        // Clear diagnostics for closed file
        let _ = self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_change_workspace_folders(&self, _params: DidChangeWorkspaceFoldersParams) {
        // Clear cached Mago path when workspace changes
        if let Ok(mut guard) = self.mago_path.write() {
            *guard = None;
        }

        // Clear results cache as paths may have changed
        if let Ok(mut cache) = self.results_cache.write() {
            cache.clear();
        }

        eprintln!("🔄 Mago LSP: Workspace changed, cleared caches");

        // Re-detect Mago configuration for new workspace
        // This will be done lazily on next Mago run
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        eprintln!("🔄 Mago LSP: Configuration change detected!");

        // Clear cached Mago path to force re-detection
        if let Ok(mut guard) = self.mago_path.write() {
            *guard = None;
            eprintln!("🗑️ Mago LSP: Cleared cached Mago path - will re-detect on next use");
        }

        // Parse the settings
        if let Some(settings) = params.settings.as_object() {
            // Look for mago settings
            if let Some(mago_settings) = settings.get("mago") {
                // Try to parse as MagoSettings
                if let Ok(parsed_settings) =
                    serde_json::from_value::<MagoSettings>(mago_settings.clone())
                {
                    // Update the rulesets if provided
                    if let Some(new_rulesets) = parsed_settings.rulesets {
                        eprintln!(
                            "⚙️ Mago LSP: Configuration changed via settings: '{}'",
                            new_rulesets
                        );
                        if let Ok(mut rulesets_guard) = self.rulesets.write() {
                            *rulesets_guard = Some(new_rulesets);
                        }
                    }
                }
            }

            // Also check for rulesets directly in settings (for compatibility)
            if let Some(rulesets_value) = settings.get("rulesets") {
                if let Some(new_rulesets) = rulesets_value.as_str() {
                    eprintln!(
                        "⚙️ Mago LSP: Configuration changed via direct rulesets setting: '{}'",
                        new_rulesets
                    );
                    if let Ok(mut rulesets_guard) = self.rulesets.write() {
                        *rulesets_guard = Some(new_rulesets.to_string());
                    }
                }
            }
        }

        // Clear results cache to force re-analysis with new config
        if let Ok(mut cache) = self.results_cache.write() {
            cache.clear();
            eprintln!("🗑️ Mago LSP: Cleared results cache after config change");
        }

        // Note: Documents will be re-analyzed on next diagnostic() call
        // No need to proactively re-run Mago on all files
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text;

        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        eprintln!(
            "📂 Mago LSP: File opened: {} ({} bytes)",
            file_name,
            text.len()
        );

        // Debug: Show first few lines of opened file
        let lines: Vec<&str> = text.lines().collect();
        eprintln!("📊 Mago LSP: Opened file has {} lines", lines.len());
        for (i, line) in lines.iter().take(5).enumerate() {
            eprintln!("  Line {}: {:?}", i + 1, line);
        }

        // Compress and store the document
        let compressed_doc = self.compress_document(&text);

        {
            let mut docs = self.open_docs.write().unwrap();
            docs.insert(uri.clone(), compressed_doc);

            // Log memory stats on significant changes
            if docs.len() % 25 == 0 {
                drop(docs); // Release lock before logging
                self.log_memory_stats();
            }
        }

        // Invalidate any cached results for this file
        if let Ok(mut cache) = self.results_cache.write() {
            let removed = cache.remove(&uri);
            eprintln!(
                "🗑️ Mago LSP: Cache invalidated for {} (URI: {}) - removed: {}",
                file_name,
                uri,
                removed.is_some()
            );
        }

        // Log memory stats periodically (every 10 files)
        if let Ok(docs) = self.open_docs.read() {
            if docs.len() % 10 == 0 {
                drop(docs); // Release lock before logging
                self.log_memory_stats();
            }
        }

        // Note: Analysis is only triggered when Zed explicitly calls diagnostic()
        // This prevents overlapping analyses and cross-file contamination
        eprintln!("📝 Mago LSP: Document stored, waiting for diagnostic request from Zed");
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();

        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        // With FULL sync, we always get the complete document content
        if let Some(change) = params.content_changes.first() {
            // Debug: Show change details
            let lines: Vec<&str> = change.text.lines().collect();
            eprintln!(
                "📝 Mago LSP: File changed: {} - now has {} lines, {} bytes",
                file_name,
                lines.len(),
                change.text.len()
            );

            // Show first 3 lines after change
            for (i, line) in lines.iter().take(3).enumerate() {
                eprintln!("  Line {}: {:?}", i + 1, line);
            }
            // Remove old compressed document to update memory tracking
            let old_size = if let Ok(docs) = self.open_docs.read() {
                docs.get(&uri).map(|doc| doc.compressed_data.len())
            } else {
                None
            };

            if let Some(size) = old_size {
                self.total_memory_usage.fetch_sub(size, Ordering::Relaxed);
            }

            // Compress and store new content
            let compressed_doc = self.compress_document(&change.text);

            let mut docs = self.open_docs.write().unwrap();
            docs.insert(uri.clone(), compressed_doc);

            // Invalidate cached results since content changed
            if let Ok(mut cache) = self.results_cache.write() {
                let removed = cache.remove(&uri);
                eprintln!(
                    "🗑️ Mago LSP: Cache invalidated after change for {} (URI: {}) - removed: {}",
                    file_name,
                    uri,
                    removed.is_some()
                );
            }
        }

        // Diagnostics will be provided via diagnostic() method
        // This reduces unnecessary Mago runs during rapid typing
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;

        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        eprintln!("💾 Mago LSP: File saved: {}", file_name);

        // Note: Diagnostics will be provided via diagnostic() method calls from Zed
        // We don't need to proactively run Mago here to avoid duplicate analysis
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> LspResult<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;
        let file_name = uri
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("unknown");

        if let Ok(file_path) = uri.to_file_path() {
            if let Some(path_str) = file_path.to_str() {
                // First check if we have cached results
                // Get current document checksum first
                let current_checksum = {
                    let docs = self.open_docs.read().unwrap();
                    docs.get(&uri).map(|doc| doc.checksum.clone())
                };

                if let Ok(cache) = self.results_cache.read() {
                    eprintln!(
                        "🔍 Mago LSP: Checking cache for {} (URI: {})",
                        file_name, uri
                    );
                    eprintln!(
                        "🔍 Mago LSP: Cache currently contains {} entries",
                        cache.len()
                    );

                    if let Some(cached) = cache.get(&uri) {
                        eprintln!(
                            "⚡ Mago LSP: Found cached results for {} (URI: {}) with {} diagnostics (age: {:.1}s)",
                            file_name,
                            uri,
                            cached.diagnostics.len(),
                            cached.generated_at.elapsed().as_secs_f64()
                        );

                        // Validate cache is still valid by checking content checksum
                        if let Some(ref checksum) = current_checksum {
                            if cached.content_checksum != *checksum {
                                eprintln!(
                                    "🔄 Mago LSP: Cache invalidated for {} - content changed (old: {}, new: {})",
                                    file_name,
                                    &cached.content_checksum[..8],
                                    &checksum[..8]
                                );
                                // Content has changed, need to re-analyze
                                drop(cache); // Release read lock before we try to write
                                if let Ok(mut cache_write) = self.results_cache.write() {
                                    cache_write.remove(&uri);
                                }
                            } else {
                                // Checksum matches, cache is valid
                                eprintln!(
                                    "✅ Mago LSP: Cache valid for {} - checksum matches",
                                    file_name
                                );

                                // Check if client has the same version
                                if let Some(previous_result_id) = params.previous_result_id {
                                    if previous_result_id == cached.result_id {
                                        eprintln!(
                                            "✅ Mago LSP: Client has current version for {}",
                                            file_name
                                        );
                                        return Ok(DocumentDiagnosticReportResult::Report(
                                            DocumentDiagnosticReport::Unchanged(
                                                RelatedUnchangedDocumentDiagnosticReport {
                                                    unchanged_document_diagnostic_report:
                                                        UnchangedDocumentDiagnosticReport {
                                                            result_id: cached.result_id.clone(),
                                                        },
                                                    related_documents: None,
                                                },
                                            ),
                                        ));
                                    }
                                }

                                // Return cached diagnostics
                                return Ok(DocumentDiagnosticReportResult::Report(
                                    DocumentDiagnosticReport::Full(
                                        RelatedFullDocumentDiagnosticReport {
                                            full_document_diagnostic_report:
                                                FullDocumentDiagnosticReport {
                                                    result_id: Some(cached.result_id.clone()),
                                                    items: cached.diagnostics.clone(),
                                                },
                                            related_documents: None,
                                        },
                                    ),
                                ));
                            }
                        } else {
                            eprintln!(
                                "⚠️ Mago LSP: No current document checksum available, invalidating cache"
                            );
                            drop(cache); // Release read lock
                            if let Ok(mut cache_write) = self.results_cache.write() {
                                cache_write.remove(&uri);
                            }
                        }
                    }
                }

                // No cached results, need to get content and run Mago
                let compressed_doc = {
                    let docs = self.open_docs.read().unwrap();
                    docs.get(&uri).cloned()
                };

                // Handle missing document (rare edge case)
                let compressed_doc = if compressed_doc.is_none() {
                    // Try to read from disk as fallback
                    match fs::read_to_string(path_str) {
                        Ok(file_content) => {
                            eprintln!(
                                "⚠️ Mago LSP: Document not in memory, reading from disk: {}",
                                file_name
                            );
                            let compressed = self.compress_document(&file_content);
                            let mut docs = self.open_docs.write().unwrap();
                            docs.insert(uri.clone(), compressed.clone());
                            Some(compressed)
                        }
                        Err(e) => {
                            eprintln!("❌ Mago LSP: Failed to read file {}: {}", file_name, e);
                            None
                        }
                    }
                } else {
                    compressed_doc
                };

                if let Some(compressed_doc) = compressed_doc {
                    // Decompress content
                    let content = match self.decompress_document(&compressed_doc) {
                        Ok(content) => {
                            // Log content details to verify we're analyzing the right file
                            eprintln!(
                                "📄 Mago LSP: Retrieved content for {} (URI: {})",
                                file_name, uri
                            );
                            eprintln!("📄 Mago LSP: Content size: {} bytes", content.len());

                            // Show first few lines to identify which file's content this is
                            let lines: Vec<&str> = content.lines().collect();
                            eprintln!("📄 Mago LSP: Content preview (first 5 lines):");
                            for (i, line) in lines.iter().take(5).enumerate() {
                                eprintln!("    Line {}: {}", i + 1, line);
                            }

                            content
                        }
                        Err(e) => {
                            eprintln!("❌ Mago LSP: Failed to decompress {}: {}", file_name, e);
                            return Ok(DocumentDiagnosticReportResult::Report(
                                DocumentDiagnosticReport::Full(
                                    RelatedFullDocumentDiagnosticReport {
                                        full_document_diagnostic_report:
                                            FullDocumentDiagnosticReport {
                                                result_id: None,
                                                items: vec![],
                                            },
                                        related_documents: None,
                                    },
                                ),
                            ));
                        }
                    };

                    // Always use stdin for content to avoid file system reads
                    if content.is_empty() {
                        eprintln!("❌ Mago LSP: No content provided for {}", file_name);
                        return Ok(DocumentDiagnosticReportResult::Report(
                            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                    result_id: None,
                                    items: vec![],
                                },
                                related_documents: None,
                            }),
                        ));
                    }

                    // Debug: Show content details
                    let lines: Vec<&str> = content.lines().collect();
                    eprintln!("📊 Mago LSP: Content has {} lines", lines.len());

                    // Show first 10 lines with line numbers
                    eprintln!("📝 Mago LSP: First 10 lines of content:");
                    for (i, line) in lines.iter().take(10).enumerate() {
                        eprintln!("  Line {}: {:?}", i + 1, line);
                    }

                    // Check for special characters
                    if content.contains('\r') {
                        eprintln!(
                            "⚠️ Mago LSP: Content contains \\r characters (Windows line endings)"
                        );
                    }
                    if content.starts_with('\u{feff}') {
                        eprintln!("⚠️ Mago LSP: Content starts with BOM (Byte Order Mark)");
                    }

                    eprintln!(
                        "📝 Mago LSP: Content size: {} bytes, {} chars",
                        content.len(),
                        content.chars().count()
                    );

                    // Debug: Calculate line count and show line ending style
                    let has_final_newline = content.ends_with('\n') || content.ends_with("\r\n");
                    eprintln!(
                        "📝 Mago LSP: Line count: {}, has final newline: {}",
                        lines.len(),
                        has_final_newline
                    );

                    let version_id = compressed_doc.checksum.clone();
                    eprintln!(
                        "📋 Mago LSP: Running Mago for {} with version: {}",
                        file_name,
                        &version_id[..16]
                    );
                    eprintln!(
                        "📋 Mago LSP: About to analyze {} with {} bytes of content",
                        file_name,
                        content.len()
                    );

                    // Create a temporary file for the PHP content
                    // Using a file instead of stdin ensures complete isolation between analyses
                    let temp_file_name = format!("php-mago-{}.php", Uuid::new_v4());
                    let temp_file_path = std::env::temp_dir().join(&temp_file_name);

                    // Write content to temporary file
                    if let Err(e) = std::fs::write(&temp_file_path, &content) {
                        eprintln!("❌ Mago LSP: Failed to write temp file: {}", e);
                        return Ok(DocumentDiagnosticReportResult::Report(
                            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                    result_id: None,
                                    items: vec![],
                                },
                                related_documents: None,
                            }),
                        ));
                    }
                    eprintln!(
                        "📁 Mago LSP: Created temporary file: {}",
                        temp_file_path.display()
                    );
                    eprintln!("📝 Mago LSP: Wrote {} bytes to temp file", content.len());

                    let lint_feature =
                        self.run_mago(&uri, path_str, "lint", &temp_file_name, &temp_file_path);
                    let analyze_feature =
                        self.run_mago(&uri, path_str, "analyze", &temp_file_name, &temp_file_path);
                    // let lint_handle = tokio::spawn(lint_feature);
                    // let analyze_handle = tokio::spawn(analyze_feature);

                    if let (Ok(lint_diagnostics), Ok(analyze_diagnostics)) =
                        tokio::join!(lint_feature, analyze_feature)
                    {
                        let diagnostics = [lint_diagnostics, analyze_diagnostics].concat();
                        eprintln!(
                            "📊 Mago LSP: Generated {} diagnostics for {}",
                            diagnostics.len(),
                            file_name
                        );

                        // Get the content checksum from the compressed document
                        let content_checksum = {
                            let docs = self.open_docs.read().unwrap();
                            docs.get(&uri)
                                .map(|doc| doc.checksum.clone())
                                .unwrap_or_else(|| String::from("unknown"))
                        };

                        // Cache the results with content checksum
                        let cached_results = CachedResults {
                            diagnostics: diagnostics.clone(),
                            result_id: version_id.clone(),
                            generated_at: Instant::now(),
                            content_checksum,
                        };

                        if let Ok(mut cache) = self.results_cache.write() {
                            eprintln!(
                                "💾 Mago LSP: Storing {} diagnostics in cache for {} (URI: {})",
                                diagnostics.len(),
                                file_name,
                                uri
                            );
                            eprintln!(
                                "💾 Mago LSP: Cache size before insert: {} entries",
                                cache.len()
                            );

                            // Log existing cache entries for debugging
                            for (cached_uri, cached_result) in cache.iter() {
                                let cached_file = cached_uri
                                    .path_segments()
                                    .and_then(|s| s.last())
                                    .unwrap_or("unknown");
                                eprintln!(
                                    "    - {} has {} cached diagnostics",
                                    cached_file,
                                    cached_result.diagnostics.len()
                                );
                            }

                            cache.insert(uri.clone(), cached_results);
                            eprintln!(
                                "💾 Mago LSP: Cache size after insert: {} entries",
                                cache.len()
                            );
                        }

                        return Ok(DocumentDiagnosticReportResult::Report(
                            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                    result_id: Some(version_id),
                                    items: diagnostics,
                                },
                                related_documents: None,
                            }),
                        ));
                    }

                    // Clean up temporary file
                    if let Err(e) = std::fs::remove_file(&temp_file_path) {
                        eprintln!("⚠️ Mago LSP: Failed to clean up temp file: {}", e);
                    }
                }
            }
        }

        // Fallback: return empty diagnostics with no version
        eprintln!(
            "⚠️ Mago LSP: Unable to generate diagnostics for {}",
            file_name
        );
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items: vec![],
                },
                related_documents: None,
            }),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let stdin = stdin();
    let stdout = stdout();

    let (service, socket) = LspService::new(|client| MagoLanguageServer::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}
