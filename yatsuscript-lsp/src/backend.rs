//! The Language Server implementation for YatsuScript.
//!
//! Provides the [`tower_lsp::LanguageServer`] trait methods.

use dashmap::DashMap;
use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{AnalysisResults, analyze_source, LspTokenVariant};
use crate::builtin_docs;

/// Core Language Server backend, holding state across clients.
pub struct YatsuBackend {
    pub client:    Client,
    pub documents: DashMap<Url, (Arc<AnalysisResults>, String)>,
}

impl YatsuBackend {
    /// Create a new backend instance for the given client.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
        }
    }

    async fn update_document(&self, uri: Url, source: String) {
        let results = Arc::new(analyze_source(&source));
        self.documents.insert(uri.clone(), (results.clone(), source));

        self.client.publish_diagnostics(uri, results.diagnostics.clone(), None).await;
    }

    fn get_word_at_pos(&self, uri: &Url, pos: Position) -> Option<String> {
        let entry  = self.documents.get(uri)?;
        let source = &entry.1;
        let lines: Vec<&str> = source.lines().collect();
        let line   = lines.get(pos.line as usize)?;
        
        let chars: Vec<char> = line.chars().collect();
        let col    = pos.character as usize;
        if col >= chars.len() { return None; }

        let mut start = col;
        while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
            start -= 1;
        }

        let mut end = col;
        while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
            end += 1;
        }

        if start == end { return None; }
        Some(chars[start..end].iter().collect())
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for YatsuBackend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::KEYWORD,
                                    SemanticTokenType::FUNCTION,
                                    SemanticTokenType::VARIABLE,
                                    SemanticTokenType::PARAMETER,
                                    SemanticTokenType::OPERATOR,
                                    SemanticTokenType::COMMENT,
                                    SemanticTokenType::STRING,
                                    SemanticTokenType::NUMBER,
                                ],
                                token_modifiers: vec![
                                    SemanticTokenModifier::DECLARATION,
                                    SemanticTokenModifier::DEFINITION,
                                ],
                            },
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client.log_message(MessageType::INFO, "YatsuScript LSP server initialized").await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.update_document(params.text_document.uri, params.text_document.text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.first() {
            self.update_document(params.text_document.uri, change.text.clone()).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri   = params.text_document_position_params.text_document.uri;
        let pos   = params.text_document_position_params.position;
        let (results, _) = match self.documents.get(&uri) {
            Some(r) => r.value().clone(),
            None    => return Ok(None),
        };

        if let Some(word) = self.get_word_at_pos(&uri, pos) {
            if let Some(doc) = builtin_docs::get_docs(&word) {
                return Ok(Some(Hover {
                    contents: HoverContents::Scalar(MarkedString::String(doc)),
                    range: None,
                }));
            }
        }

        let decl = results.declarations.iter().find(|d| {
            d.range.start.line <= pos.line 
                && d.range.end.line >= pos.line 
                && (d.range.start.line < pos.line || d.range.start.character <= pos.character)
                && (d.range.end.line > pos.line || d.range.end.character >= pos.character)
        });

        if let Some(d) = decl {
            let msg = match d.kind {
                SymbolKind::FUNCTION => format!("(function) {}", d.name),
                SymbolKind::VARIABLE => format!("(variable) {}", d.name),
                _ => d.name.clone(),
            };
            return Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(msg)),
                range: Some(d.range),
            }));
        }

        Ok(None)
    }

    async fn completion(&self, _: CompletionParams) -> Result<Option<CompletionResponse>> {
        let mut items = Vec::new();

        for (name, detail) in builtin_docs::ITER {
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some(detail.to_string()),
                ..Default::default()
            });
        }
        
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn goto_definition(&self, params: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let word = match self.get_word_at_pos(&uri, pos) {
            Some(w) => w,
            None    => return Ok(None),
        };

        let (results, _) = match self.documents.get(&uri) {
            Some(r) => r.value().clone(),
            None    => return Ok(None),
        };

        for decl in &results.declarations {
            if decl.name == word {
                return Ok(Some(GotoDefinitionResponse::Scalar(Location::new(uri, decl.range))));
            }
        }

        Ok(None)
    }

    async fn document_symbol(&self, params: DocumentSymbolParams) -> Result<Option<DocumentSymbolResponse>> {
        let uri     = params.text_document.uri;
        let (results, _) = match self.documents.get(&uri) {
            Some(r) => r.value().clone(),
            None    => return Ok(None),
        };

        let syms: Vec<SymbolInformation> = results.declarations.iter().map(|d| {
            #[allow(deprecated)]
            SymbolInformation {
                name: d.name.clone(),
                kind: d.kind,
                location: Location::new(uri.clone(), d.range),
                container_name: None,
                tags: None,
                deprecated: None,
            }
        }).collect();

        Ok(Some(DocumentSymbolResponse::Flat(syms)))
    }

    async fn semantic_tokens_full(&self, params: SemanticTokensParams) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let (results, _) = match self.documents.get(&uri) {
            Some(r) => r.value().clone(),
            None    => return Ok(None),
        };

        let mut data = Vec::new();
        let mut prev_line = 0;
        let mut prev_char = 0;

        for token in &results.tokens {
            let type_idx = match token.variant {
                LspTokenVariant::Keyword   => 0,
                LspTokenVariant::Function  => 1,
                LspTokenVariant::Variable  => 2,
                LspTokenVariant::Parameter => 3,
                LspTokenVariant::Operator  => 4,
                LspTokenVariant::Comment   => 5,
                LspTokenVariant::String    => 6,
                LspTokenVariant::Number    => 7,
                LspTokenVariant::Other     => continue,
            };

            let delta_line = token.line - prev_line;
            let delta_char = if delta_line == 0 { token.char - prev_char } else { token.char };

            data.push(SemanticToken {
                delta_line,
                delta_start: delta_char,
                length: token.len,
                token_type: type_idx,
                token_modifiers_bitset: 0,
            });

            prev_line = token.line;
            prev_char = token.char;
        }

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
