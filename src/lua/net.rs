use std::{
    rc::{Rc, Weak},
    time::Instant,
};

use mlua::{Function, Lua, RegistryKey, Table, UserData, UserDataMethods, Value};

use super::{Runtime, State, live_state, log_failure};
use crate::net::Listener;

pub(super) struct Active {
    socket: Listener,
    callback: RegistryKey,
    token: Rc<()>,
}

struct Handle {
    state: Weak<State>,
    token: Rc<()>,
}

impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("close", |lua, handle, ()| {
            if let Some(state) = handle.state.upgrade() {
                let mut listener = state.listener.borrow_mut();
                if listener
                    .as_ref()
                    .is_some_and(|active| Rc::ptr_eq(&active.token, &handle.token))
                {
                    *listener = None;
                }
            }
            lua.expire_registry_values();
            Ok(())
        });
    }
}

pub(super) fn install(lua: &Lua, tvi: &Table, state: Weak<State>) -> mlua::Result<()> {
    let api = lua.create_table()?;
    api.set(
        "listen_local",
        lua.create_function(move |lua, (port, callback): (Value, Function)| {
            let Value::Integer(port @ 1..=65535) = port else {
                return Ok((None, Some("port must be an integer from 1 to 65535".into())));
            };
            let state = live_state(&state)?;
            if state.listener.borrow().is_some() {
                return Ok((None, Some("a local listener is already active".into())));
            }
            let socket = match Listener::bind(u16::try_from(port).map_err(mlua::Error::external)?) {
                Ok(socket) => socket,
                Err(error) => {
                    return Ok((
                        None,
                        Some(format!("cannot listen on 127.0.0.1:{port}: {error}")),
                    ));
                }
            };
            let token = Rc::new(());
            let handle = lua.create_userdata(Handle {
                state: Rc::downgrade(&state),
                token: Rc::clone(&token),
            })?;
            let callback = lua.create_registry_value(callback)?;
            *state.listener.borrow_mut() = Some(Active {
                socket,
                callback,
                token,
            });
            Ok((Some(handle), None))
        })?,
    )?;
    tvi.set("net", api)
}

impl Runtime {
    pub(crate) fn poll_net(&self, now: Instant, can_deliver: impl Fn() -> bool) -> bool {
        let (token, lines) = {
            let mut active = self.state.listener.borrow_mut();
            let Some(active) = active.as_mut() else {
                return false;
            };
            (
                Rc::clone(&active.token),
                active.socket.poll(now, can_deliver()),
            )
        };
        let mut redraw = false;
        for line in lines {
            if !can_deliver() {
                break;
            }
            let callback = {
                let active = self.state.listener.borrow();
                let Some(active) = active
                    .as_ref()
                    .filter(|active| Rc::ptr_eq(&active.token, &token))
                else {
                    break;
                };
                self.lua.registry_value::<Function>(&active.callback)
            };
            match callback.and_then(|callback| callback.call::<Value>(line)) {
                Ok(Value::Boolean(false)) => {}
                Ok(_) => redraw = true,
                Err(error) => {
                    log_failure("net_line", "continue", &error, None);
                    // The Lua error can contain data received over the socket.
                    self.state.host.notify("tvi.net: on_line callback failed");
                    redraw = true;
                }
            }
        }
        redraw
    }
}
