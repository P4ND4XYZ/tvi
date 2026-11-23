use std::{path::PathBuf, rc::Weak};

use mlua::{AnyUserData, Error, Lua, Table, UserData, UserDataMethods, Value};

use super::{Host, State, ViewHandle, live_state};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    Markdown,
    Pdf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentInfo {
    pub epoch: u64,
    pub kind: DocumentKind,
}

pub(super) fn for_view(lua: &Lua, view: &ViewHandle) -> mlua::Result<Option<AnyUserData>> {
    let state = live_state(&view.state)?;
    let Some(info) = state.host.document(view.id).map_err(Error::runtime)? else {
        return Ok(None);
    };
    let handle = DocumentHandle {
        view: view.id,
        info,
        state: view.state.clone(),
    };
    match info.kind {
        DocumentKind::Markdown => lua.create_userdata(MarkdownHandle(handle)),
        DocumentKind::Pdf => lua.create_userdata(PdfHandle(handle)),
    }
    .map(Some)
}

struct DocumentHandle {
    view: u64,
    info: DocumentInfo,
    state: Weak<State>,
}

impl DocumentHandle {
    fn with_host<T>(
        &self,
        f: impl FnOnce(&dyn Host, u64) -> Result<T, String>,
    ) -> Result<T, String> {
        let state = self.state.upgrade().ok_or("Lua runtime has expired")?;
        if state.host.document(self.view)? != Some(self.info) {
            return Err("document is no longer displayed".into());
        }
        f(state.host.as_ref(), self.view)
    }
}

fn path_result(
    result: Result<PathBuf, String>,
    invalid_utf8: &str,
) -> (Option<String>, Option<String>) {
    match result.and_then(|path| {
        path.into_os_string()
            .into_string()
            .map_err(|_| invalid_utf8.to_owned())
    }) {
        Ok(path) => (Some(path), None),
        Err(error) => (None, Some(error)),
    }
}

fn add_common<T: 'static, M: UserDataMethods<T>>(
    methods: &mut M,
    kind: &'static str,
    handle: fn(&T) -> &DocumentHandle,
) {
    methods.add_method("type", move |_, doc, ()| {
        handle(doc)
            .with_host(|_, _| Ok(kind))
            .map_err(Error::runtime)
    });
    methods.add_method("path", move |_, doc, ()| {
        Ok(path_result(
            handle(doc).with_host(|host, view| host.document_path(view)),
            "document path is not valid UTF-8",
        ))
    });
    methods.add_method("project_root", move |_, doc, opts: Option<Table>| {
        let markers = opts
            .map(|opts| opts.get::<Option<Vec<String>>>("root_markers"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| vec![".git".into()]);
        Ok(path_result(
            handle(doc).with_host(|host, view| host.project_root(view, &markers)),
            "project root is not valid UTF-8",
        ))
    });
}

struct MarkdownHandle(DocumentHandle);

impl UserData for MarkdownHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        add_common(methods, "md", |doc: &Self| &doc.0);
        methods.add_method("links", |lua, doc, ()| {
            let links = doc
                .0
                .with_host(|host, view| host.links(view))
                .map_err(Error::runtime)?;
            let list = lua.create_table_with_capacity(links.len(), 0)?;
            for (index, link) in links.into_iter().enumerate() {
                let candidate = lua.create_table()?;
                candidate.set("id", link.id)?;
                candidate.set("label", link.label)?;
                candidate.set("target", link.target)?;
                list.raw_set(index + 1, candidate)?;
            }
            Ok(list)
        });
        methods.add_method("resolve_link", |_, doc, id: u64| {
            Ok(path_result(
                doc.0.with_host(|host, view| host.resolve_link(view, id)),
                "link path is not valid UTF-8",
            ))
        });
        methods.add_method("scroll_to_line", |_, doc, line: Value| {
            let Value::Integer(line) = line else {
                return Err(Error::runtime("scroll_to_line: line must be an integer"));
            };
            doc.0
                .with_host(|host, view| host.scroll_to_line(view, line))
                .map_err(Error::runtime)
        });
    }
}

struct PdfHandle(DocumentHandle);

impl UserData for PdfHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        add_common(methods, "pdf", |doc: &Self| &doc.0);
        methods.add_method("zoom", |_, doc, ()| {
            doc.0
                .with_host(|host, view| host.pdf_zoom(view))
                .map_err(Error::runtime)
        });
        methods.add_method("set_zoom", |_, doc, scale: Value| {
            let scale = match scale {
                Value::Number(scale) => scale,
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "integers too large for exact conversion exceed the display limits"
                )]
                Value::Integer(scale) => scale as f64,
                _ => {
                    return Ok((
                        false,
                        Some("zoom scale must be a finite positive number".into()),
                    ));
                }
            };
            if !scale.is_finite() || scale <= 0.0 {
                return Ok((
                    false,
                    Some("zoom scale must be a finite positive number".into()),
                ));
            }
            Ok(
                match doc.0.with_host(|host, view| host.set_pdf_zoom(view, scale)) {
                    Ok(()) => (true, None),
                    Err(error) => (false, Some(error)),
                },
            )
        });
        methods.add_method("pan_x", |_, doc, columns: Value| {
            let Value::Integer(columns) = columns else {
                return Err(Error::runtime("pan_x: columns must be an integer"));
            };
            doc.0
                .with_host(|host, view| host.pan_pdf_x(view, columns))
                .map_err(Error::runtime)
        });
    }
}
