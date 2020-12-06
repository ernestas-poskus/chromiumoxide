use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use fnv::FnvHashMap;
use futures::channel::mpsc::Receiver;
use futures::channel::oneshot::Sender as OneshotSender;
use futures::stream::{Fuse, Stream, StreamExt};
use futures::task::{Context, Poll};

use chromiumoxid_types::Request as CdpRequest;
use chromiumoxid_types::{CallId, Command, CommandResponse, Message, Method, Response};
pub(crate) use page::PageInner;

use crate::handler::frame::FrameNavigationRequest;
use crate::handler::frame::{NavigationError, NavigationId, NavigationOk};
use crate::handler::target::{TargetEvent, TargetMessage};
use crate::{
    browser::CommandMessage,
    cdp::{
        browser_protocol::{browser::*, target::*},
        events::{CdpEvent, CdpEventMessage},
    },
    conn::Connection,
    error::CdpError,
    handler::{browser::BrowserContext, job::PeriodicJob, session::Session, target::Target},
    page::Page,
};

/// Standard timeout in MS
pub const REQUEST_TIMEOUT: u64 = 30000;

mod browser;
mod cmd;
pub mod emulation;
pub mod frame;
mod job;
pub mod network;
mod page;
mod session;
pub mod target;
mod viewport;

// puppeteer
pub struct Handler {
    /// Commands that are being processed await a response from the chromium
    /// instance
    pending_commands: FnvHashMap<CallId, (PendingRequest, Instant)>,
    /// Connection to the browser instance
    from_browser: Fuse<Receiver<HandlerMessage>>,
    // default_ctx: BrowserContext,
    contexts: HashMap<BrowserContextId, BrowserContext>,
    pages: Vec<(Fuse<Receiver<HandlerMessage>>, Arc<PageInner>)>,
    /// Used to loop over all targets in a consistent manner
    target_ids: Vec<TargetId>,
    /// The created and attached targets
    targets: HashMap<TargetId, Target>,
    navigations: FnvHashMap<NavigationId, NavigationRequest>,
    /// Keeps track of all the current active sessions
    ///
    /// There can be multiple sessions per target.
    sessions: HashMap<SessionId, Session>,
    /// The websocket connection to the chromium instance
    conn: Connection<CdpEventMessage>,
    evict_command_timeout: PeriodicJob,
    /// The internal identifier for a specific navigation
    next_navigation_id: usize,
}

impl Handler {
    pub(crate) fn new(mut conn: Connection<CdpEventMessage>, rx: Receiver<HandlerMessage>) -> Self {
        let discover = SetDiscoverTargetsParams::new(true);
        let _ = conn.submit_command(
            discover.identifier(),
            None,
            serde_json::to_value(discover).unwrap(),
        );

        Self {
            pending_commands: Default::default(),
            from_browser: rx.fuse(),
            contexts: Default::default(),
            pages: Default::default(),
            target_ids: Default::default(),
            targets: Default::default(),
            navigations: Default::default(),
            sessions: Default::default(),
            conn,
            evict_command_timeout: Default::default(),
            next_navigation_id: 0,
        }
    }

    /// Return the target with the matching `target_id`
    pub fn get_target(&self, target_id: &TargetId) -> Option<&Target> {
        self.targets.get(target_id)
    }

    /// Iterator over all currently attached targets
    pub fn targets(&self) -> impl Iterator<Item = &Target> + '_ {
        self.targets.values()
    }

    /// received a response to a navigation request like `Page.navigate`
    fn on_navigation_response(&mut self, id: NavigationId, resp: Response) {
        if let Some(nav) = self.navigations.remove(&id) {
            match nav {
                NavigationRequest::Goto(mut nav) => {
                    if nav.navigated {
                        let _ = nav.tx.send(Ok(resp));
                    } else {
                        nav.set_response(resp);
                        self.navigations.insert(id, NavigationRequest::Goto(nav));
                    }
                }
            }
        }
    }

    fn on_navigation_lifecycle_completed(&mut self, res: Result<NavigationOk, NavigationError>) {
        match res {
            Ok(ok) => {
                let id = *ok.navigation_id();
                if let Some(nav) = self.navigations.remove(&id) {
                    match nav {
                        NavigationRequest::Goto(mut nav) => {
                            if let Some(resp) = nav.response.take() {
                                let _ = nav.tx.send(Ok(resp));
                            } else {
                                nav.set_navigated();
                                self.navigations.insert(id, NavigationRequest::Goto(nav));
                            }
                        }
                    }
                }
            }
            Err(err) => {
                if let Some(nav) = self.navigations.remove(err.navigation_id()) {
                    match nav {
                        NavigationRequest::Goto(nav) => {
                            let _ = nav.tx.send(Err(err.into()));
                        }
                    }
                }
            }
        }
    }

    /// Received a response to a request
    fn on_response(&mut self, resp: Response) {
        if let Some((req, _)) = self.pending_commands.remove(&resp.id) {
            match req {
                PendingRequest::CreateTarget(tx) => {
                    match to_command_response::<CreateTargetParams>(resp) {
                        Ok(resp) => {
                            if let Some(target) = self.targets.get_mut(&resp.target_id) {
                                // move the sender to the target that sends its page once
                                // initialized
                                target.set_initiator(tx);
                            } else {
                                // TODO can this even happen?
                                panic!("Created target not present")
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err)).ok();
                        }
                    }
                }
                PendingRequest::Navigate(id) => {
                    self.on_navigation_response(id, resp);
                }
                PendingRequest::ExternalCommand(tx) => {
                    let _ = tx.send(Ok(resp)).ok();
                }
                PendingRequest::InternalCommand(target_id) => {
                    if let Some(target) = self.targets.get_mut(&target_id) {
                        target.on_response(resp);
                    }
                }
            }
        }
    }

    pub(crate) fn submit_external_command(
        &mut self,
        msg: CommandMessage,
        now: Instant,
    ) -> Result<(), CdpError> {
        let call_id = self
            .conn
            .submit_command(msg.method, msg.session_id, msg.params)?;
        self.pending_commands
            .insert(call_id, (PendingRequest::ExternalCommand(msg.sender), now));
        Ok(())
    }

    pub(crate) fn submit_internal_command(
        &mut self,
        target_id: TargetId,
        req: CdpRequest,
        now: Instant,
    ) -> Result<(), CdpError> {
        let call_id =
            self.conn
                .submit_command(req.method, req.session_id.map(Into::into), req.params)?;
        self.pending_commands
            .insert(call_id, (PendingRequest::InternalCommand(target_id), now));
        Ok(())
    }

    fn submit_navigation(&mut self, id: NavigationId, req: CdpRequest, now: Instant) {
        let call_id = self
            .conn
            .submit_command(req.method, req.session_id.map(Into::into), req.params)
            .unwrap();

        self.pending_commands
            .insert(call_id, (PendingRequest::Navigate(id), now));
    }

    /// Process a message received by the target
    fn on_target_message(&mut self, target: &mut Target, msg: TargetMessage, now: Instant) {
        match msg {
            TargetMessage::Command(msg) => {
                if msg.is_navigation() {
                    let (req, tx) = msg.split();
                    let id = self.next_navigation_id();
                    target.goto(FrameNavigationRequest::new(id, req));
                    self.navigations
                        .insert(id, NavigationRequest::Goto(NavigationInProgress::new(tx)));
                } else {
                    let _ = self.submit_external_command(msg, now);
                }
            }
        }
    }

    fn next_navigation_id(&mut self) -> NavigationId {
        let id = NavigationId(self.next_navigation_id);
        self.next_navigation_id = self.next_navigation_id.wrapping_add(1);
        id
    }

    /// Create a new page and send it to the receiver
    fn create_page(
        &mut self,
        params: CreateTargetParams,
        tx: OneshotSender<Result<Page, CdpError>>,
    ) {
        let method = params.identifier();
        match serde_json::to_value(params) {
            Ok(params) => match self.conn.submit_command(method, None, params) {
                Ok(call_id) => {
                    self.pending_commands
                        .insert(call_id, (PendingRequest::CreateTarget(tx), Instant::now()));
                }
                Err(err) => {
                    let _ = tx.send(Err(err.into())).ok();
                }
            },
            Err(err) => {
                let _ = tx.send(Err(err.into())).ok();
            }
        }
    }

    fn on_event(&mut self, event: CdpEventMessage) {
        if let Some(ref session_id) = event.session_id {
            if let Some(session) = self.sessions.get(session_id) {
                if let Some(target) = self.targets.get_mut(session.target_id()) {
                    return target.on_event(event);
                }
            }
        }
        match event.params {
            CdpEvent::TargetTargetCreated(ev) => self.on_target_created(ev),
            CdpEvent::TargetAttachedToTarget(ev) => self.on_attached_to_target(ev),
            CdpEvent::TargetTargetDestroyed(ev) => self.on_target_destroyed(ev),
            CdpEvent::TargetDetachedFromTarget(ev) => self.on_detached_from_target(ev),
            _ => {}
        }
    }

    /// Fired when a new target was created on the chromium instance
    ///
    /// Creates a new `Target` instance and keeps track of it
    fn on_target_created(&mut self, event: EventTargetCreated) {
        let target = Target::new(event.target_info);
        self.target_ids.push(target.target_id().clone());
        self.targets.insert(target.target_id().clone(), target);
    }

    fn on_attached_to_target(&mut self, event: EventAttachedToTarget) {
        let session = Session::new(
            event.session_id,
            event.target_info.r#type,
            event.target_info.target_id,
        );
        if let Some(target) = self.targets.get_mut(session.target_id()) {
            target.set_session_id(session.session_id().clone())
        }
    }

    /// The session was detached from target.
    /// Can be issued multiple times per target if multiple session have been
    /// attached to it.
    fn on_detached_from_target(&mut self, event: EventDetachedFromTarget) {
        // remove the session
        if let Some(session) = self.sessions.remove(&event.session_id) {
            if let Some(target) = self.targets.get_mut(session.target_id()) {
                target.session_id().take();
            }
        }
    }

    fn on_target_destroyed(&mut self, event: EventTargetDestroyed) {
        if let Some(target) = self.targets.remove(&event.target_id) {
            // TODO shutdown?
            if let Some(session) = target.session_id() {
                self.sessions.remove(session);
            }
        }
    }
}

impl Stream for Handler {
    type Item = Result<CdpEventMessage, CdpError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();
        let now = Instant::now();

        // temporary pinning of the browser receiver should be safe as we are pinning
        // through the already pinned self. with the receivers we can also
        // safely ignore exhaustion as those are fused.
        while let Poll::Ready(Some(msg)) = Pin::new(&mut pin.from_browser).poll_next(cx) {
            match msg {
                HandlerMessage::Command(cmd) => {
                    pin.submit_external_command(cmd, now).unwrap();
                }
                HandlerMessage::CreatePage(params, tx) => {
                    pin.create_page(params, tx);
                }
                HandlerMessage::GetPages(tx) => {
                    let pages: Vec<_> = pin
                        .targets
                        .values_mut()
                        .filter_map(|target| target.get_or_create_page())
                        .map(|page| Page::from(page.clone()))
                        .collect();
                    let _ = tx.send(pages);
                }
                HandlerMessage::Subscribe => {}
            }
        }

        for n in (0..pin.target_ids.len()).rev() {
            let target_id = pin.target_ids.swap_remove(n);
            if let Some((id, mut target)) = pin.targets.remove_entry(&target_id) {
                while let Some(event) = target.poll(cx, now) {
                    match event {
                        TargetEvent::Request(req) => {
                            let _ =
                                pin.submit_internal_command(target.target_id().clone(), req, now);
                        }
                        TargetEvent::RequestTimeout(_) => {
                            // TODO close and remove
                            // continue 'outer;
                        }
                        TargetEvent::Message(msg) => {
                            pin.on_target_message(&mut target, msg, now);
                        }
                        TargetEvent::NavigationRequest(id, req) => {
                            pin.submit_navigation(id, req, now);
                        }
                        TargetEvent::NavigationResult(res) => {
                            pin.on_navigation_lifecycle_completed(res)
                        }
                    }
                }

                pin.targets.insert(id, target);
                pin.target_ids.push(target_id);
            }
        }

        while let Poll::Ready(Some(ev)) = Pin::new(&mut pin.conn).poll_next(cx) {
            match ev {
                Ok(Message::Response(resp)) => pin.on_response(resp),
                Ok(Message::Event(ev)) => {
                    pin.on_event(ev);
                }
                Err(err) => return Poll::Ready(Some(Err(err))),
            }
        }

        if pin.evict_command_timeout.is_ready(cx) {
            // TODO evict all commands that timed out
        }

        Poll::Pending
    }
}

#[derive(Debug)]
pub struct NavigationInProgress<T> {
    /// Marker to indicate whether a navigation lifecycle has completed
    navigated: bool,
    /// The response of the issued navigation request
    response: Option<Response>,
    /// Sender towards the receiver who initiated the navigation request
    tx: OneshotSender<T>,
}

impl<T> NavigationInProgress<T> {
    fn new(tx: OneshotSender<T>) -> Self {
        Self {
            navigated: false,
            response: None,
            tx,
        }
    }

    fn set_response(&mut self, resp: Response) {
        self.response = Some(resp);
    }

    fn set_navigated(&mut self) {
        self.navigated = true;
    }
}

#[derive(Debug)]
enum NavigationRequest {
    Goto(NavigationInProgress<Result<Response, CdpError>>),
}

impl NavigationRequest {
    fn set_response(&mut self, response: Response) {
        match self {
            NavigationRequest::Goto(nav) => nav.set_response(response),
        }
    }
}

#[derive(Debug)]
enum PendingRequest {
    CreateTarget(OneshotSender<Result<Page, CdpError>>),
    Navigate(NavigationId),
    ExternalCommand(OneshotSender<Result<Response, CdpError>>),
    InternalCommand(TargetId),
}

/// Events used internally to communicate with the handler, which are executed
/// in the background
// TODO rename to BrowserMessage
#[derive(Debug)]
pub(crate) enum HandlerMessage {
    CreatePage(CreateTargetParams, OneshotSender<Result<Page, CdpError>>),
    GetPages(OneshotSender<Vec<Page>>),
    Command(CommandMessage),
    Subscribe,
}

pub(crate) fn to_command_response<T: Command>(
    resp: Response,
) -> Result<CommandResponse<T::Response>, CdpError> {
    if let Some(res) = resp.result {
        let result = serde_json::from_value(res)?;
        Ok(CommandResponse {
            id: resp.id,
            result,
            method: resp.method,
        })
    } else if let Some(err) = resp.error {
        Err(err.into())
    } else {
        Err(CdpError::NoResponse)
    }
}
