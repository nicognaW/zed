use collections::HashMap;
use futures::future::join_all;
use gpui::{App, Entity, Task};
use itertools::Itertools;
use language::{Buffer, BufferSnapshot};
use lsp::LanguageServerId;
use project::lsp_store::{LspDocumentLink, ResolvedDocumentLink};
use settings::Settings;
use text::BufferId;
use ui::Context;

use crate::{Editor, LSP_REQUEST_DEBOUNCE_TIMEOUT, editor_settings::EditorSettings};

pub(super) struct LspDocumentLinks {
    pub(super) enabled: bool,
    pub(super) per_buffer: HashMap<BufferId, HashMap<LanguageServerId, Vec<LspDocumentLink>>>,
    pub(super) refresh_task: Task<()>,
}

impl LspDocumentLinks {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            enabled: EditorSettings::get_global(cx).lsp_document_links,
            per_buffer: HashMap::default(),
            refresh_task: Task::ready(()),
        }
    }
}

impl Editor {
    pub(super) fn refresh_document_links(
        &mut self,
        for_buffer: Option<BufferId>,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_data_enabled() || !self.lsp_document_links.enabled {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };

        let buffers_to_query = self
            .visible_buffers(cx)
            .into_iter()
            .filter(|buffer| self.is_lsp_relevant(buffer.read(cx).file(), cx))
            .chain(for_buffer.and_then(|id| self.buffer.read(cx).buffer(id)))
            .filter(|buffer| {
                let id = buffer.read(cx).remote_id();
                for_buffer.is_none_or(|target| target == id)
                    && self.registered_buffers.contains_key(&id)
            })
            .unique_by(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();
        if buffers_to_query.is_empty() {
            self.lsp_document_links.refresh_task = Task::ready(());
            return;
        }

        self.lsp_document_links.refresh_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                .await;

            let Some(tasks_for_buffers) = editor
                .update(cx, |_, cx| {
                    project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
                        buffers_to_query
                            .into_iter()
                            .map(|buffer| {
                                let buffer_id = buffer.read(cx).remote_id();
                                let task = lsp_store.fetch_document_links(&buffer, cx);
                                async move { (buffer_id, task.await) }
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .ok()
            else {
                return;
            };

            let new_links_for_buffers = join_all(tasks_for_buffers).await;
            editor
                .update(cx, |editor, _| {
                    for (buffer_id, links) in new_links_for_buffers {
                        let Some(links) = links else {
                            continue;
                        };
                        if links.is_empty() {
                            editor.lsp_document_links.per_buffer.remove(&buffer_id);
                        } else {
                            let mut by_server = HashMap::default();
                            for link in links {
                                by_server
                                    .entry(link.server_id)
                                    .or_insert_with(Vec::new)
                                    .push(link);
                            }
                            editor
                                .lsp_document_links
                                .per_buffer
                                .insert(buffer_id, by_server);
                        }
                    }
                })
                .ok();
        });
    }

    /// Returns a task yielding the resolved document links covering `position`
    /// in `buffer`. Resolution is deduplicated through `LspStore`'s
    /// per-`(server_id, range)` `Shared` task; the editor's mirror is updated
    /// when the resolves complete so subsequent renders/hovers find resolved
    /// data without re-issuing requests.
    ///
    /// Returns `None` when nothing is cached at `position` so callers can skip
    /// spawning anything.
    pub fn document_links_at(
        &mut self,
        buffer: Entity<Buffer>,
        position: text::Anchor,
        cx: &mut Context<Self>,
    ) -> Option<Task<Vec<LspDocumentLink>>> {
        let buffer_id = buffer.read(cx).remote_id();
        let snapshot = buffer.read(cx).snapshot();
        let matches = self
            .lsp_document_links
            .per_buffer
            .get(&buffer_id)?
            .values()
            .flatten()
            .filter(|link| link_contains(link, &position, &snapshot))
            .cloned()
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return None;
        }

        let project = self.project.clone()?;
        let mut resolved_links = Vec::with_capacity(matches.len());
        let mut pending = Vec::new();
        project.update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                for link in matches {
                    match lsp_store.resolved_document_link(
                        &buffer,
                        link.server_id,
                        link.range.clone(),
                        cx,
                    ) {
                        Some(ResolvedDocumentLink::Resolved(resolved)) => {
                            resolved_links.push(resolved);
                        }
                        Some(ResolvedDocumentLink::Resolving(task)) => {
                            pending.push(task);
                        }
                        None => {
                            // Cache no longer holds the link (likely a version
                            // bump between the mirror snapshot and now); skip.
                        }
                    }
                }
            })
        });

        if pending.is_empty() {
            return Some(Task::ready(resolved_links));
        }

        Some(cx.spawn(async move |editor, cx| {
            resolved_links.extend(join_all(pending).await.into_iter().flatten());
            editor
                .update(cx, |editor, cx| {
                    if let Some(by_server) =
                        editor.lsp_document_links.per_buffer.get_mut(&buffer_id)
                    {
                        for resolved in &resolved_links {
                            if let Some(slot) =
                                by_server.get_mut(&resolved.server_id).and_then(|links| {
                                    links.iter_mut().find(|link| link.range == resolved.range)
                                })
                            {
                                *slot = resolved.clone();
                            }
                        }
                    }
                    cx.notify();
                })
                .ok();

            resolved_links
        }))
    }
}

fn link_contains(
    link: &LspDocumentLink,
    position: &text::Anchor,
    snapshot: &BufferSnapshot,
) -> bool {
    link.range.start.cmp(position, snapshot).is_le()
        && link.range.end.cmp(position, snapshot).is_ge()
}
