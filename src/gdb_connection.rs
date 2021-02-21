//! Much of this implementation is based on the documentation at
//!
//! http://sourceware.org/gdb/onlinedocs/gdb/Packets.html

use crate::{
    gdb_register::GdbRegister,
    log::LogLevel::{LogDebug, LogError, LogInfo, LogWarn},
    registers::MAX_REG_SIZE_BYTES,
    remote_ptr::{RemotePtr, Void},
    replay_timeline::RunDirection,
    scoped_fd::ScopedFd,
    session::SessionSharedPtr,
    sig::Sig,
};
use libc::pid_t;
use nix::{
    errno::Errno,
    poll::{poll, PollFd, PollFlags},
    sys::socket::accept,
    unistd,
    Error,
};
use std::{
    ffi::{OsStr, OsString},
    fmt::{self, Display},
    io::Write,
    mem::size_of_val,
    os::unix::ffi::OsStrExt,
};

include!(concat!(
    env!("OUT_DIR"),
    "/gdb_request_bindings_generated.rs"
));

macro_rules! parser_assert {
    ( $x:expr ) => {{
        // DIFF NOTE: In rr the logic is ever so slightly different.
        // In rr there is a fputs followed by a debug_assert and exit.
        assert!($x, "Failed to parse gdb request");
    }};
}

macro_rules! parser_assert_eq {
    ( $x:expr, $y:expr ) => {{
        assert_eq!($x, $y, "Failed to parse gdb request");
    }};
}

const INTERRUPT_CHAR: u8 = b'\x03';

/// Represents a possibly-undefined register `name`.  `size` indicates how
/// many bytes of `value` are valid, if any.
#[derive(Clone, Default, Debug)]
pub struct GdbRegisterValue {
    pub name: GdbRegister,
    pub value: GdbRegisterValueData,
    pub defined: bool,
    pub size: usize,
}

#[derive(Clone, Debug)]
pub enum GdbRegisterValueData {
    Value([u8; GdbRegisterValue::MAX_SIZE]),
    Value1(u8),
    Value2(u16),
    Value4(u32),
    Value8(u64),
}

impl Default for GdbRegisterValueData {
    fn default() -> Self {
        // Pick something arbitrary
        GdbRegisterValueData::Value8(0)
    }
}

impl GdbRegisterValue {
    // Max register size
    const MAX_SIZE: usize = MAX_REG_SIZE_BYTES;

    pub fn value1(&self) -> u8 {
        match self.value {
            GdbRegisterValueData::Value1(v) => v,
            _ => panic!("Unexpected GdbRegisterValue: {:?}", self),
        }
    }
    pub fn value2(&self) -> u16 {
        match self.value {
            GdbRegisterValueData::Value2(v) => v,
            _ => panic!("Unexpected GdbRegisterValue: {:?}", self),
        }
    }
    pub fn value4(&self) -> u32 {
        match self.value {
            GdbRegisterValueData::Value4(v) => v,
            _ => panic!("Unexpected GdbRegisterValue: {:?}", self),
        }
    }
    pub fn value8(&self) -> u64 {
        match self.value {
            GdbRegisterValueData::Value8(v) => v,
            _ => panic!("Unexpected GdbRegisterValue: {:?}", self),
        }
    }
    pub fn value(&self) -> Vec<u8> {
        match self.value {
            GdbRegisterValueData::Value(v) => v[0..self.size].to_owned(),
            _ => panic!("Unexpected GdbRegisterValue: {:?}", self),
        }
    }
}

/// Descriptor for task.  Note: on linux, we can uniquely identify any thread
/// by its `tid` (in rd's pid namespace).
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct GdbThreadId {
    pub pid: pid_t,
    pub tid: pid_t,
}

impl Default for GdbThreadId {
    fn default() -> Self {
        GdbThreadId { pid: -1, tid: -1 }
    }
}

impl GdbThreadId {
    const ANY: GdbThreadId = GdbThreadId::new(0, 0);
    const ALL: GdbThreadId = GdbThreadId::new(-1, -1);

    const fn new(pid: pid_t, tid: pid_t) -> Self {
        GdbThreadId { pid, tid }
    }
}

impl Display for GdbThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "{}.{}", self.pid, self.tid)
    }
}

#[derive(Default, Clone)]
pub struct GdbRequest {
    pub type_: GdbRequestType,
    pub value: GdbRequestValue,
    pub target: GdbThreadId,
    pub suppress_debugger_stop: bool,
}

#[derive(Clone)]
pub enum GdbRequestValue {
    GdbRequestNone,
    GdbRequestMem(gdb_request::Mem),
    GdbRequestWatch(gdb_request::Watch),
    GdbRequestRestart(gdb_request::Restart),
    GdbRequestRegisterValue(GdbRegisterValue),
    GdbRequestText(OsString),
    GdbRequestCont(gdb_request::Cont),
    GdbRequestTls(gdb_request::Tls),
    GdbRequestSymbol(gdb_request::Symbol),
    GdbRequestFileSetfs(gdb_request::FileSetfs),
    GdbRequestFileOpen(gdb_request::FileOpen),
    GdbRequestFilePread(gdb_request::FilePread),
    GdbRequestFileClose(gdb_request::FileClose),
}

impl Default for GdbRequestValue {
    fn default() -> Self {
        Self::GdbRequestNone
    }
}

impl GdbRequest {
    pub fn new(maybe_type: Option<GdbRequestType>) -> GdbRequest {
        let type_ = maybe_type.unwrap_or(DREQ_NONE);
        let value = match type_ {
            DREQ_NONE => GdbRequestValue::GdbRequestNone,
            t if t >= DREQ_MEM_FIRST && t <= DREQ_MEM_LAST => {
                GdbRequestValue::GdbRequestMem(Default::default())
            }
            t if t >= DREQ_WATCH_FIRST && t <= DREQ_WATCH_LAST => {
                GdbRequestValue::GdbRequestWatch(Default::default())
            }
            t if t >= DREQ_REG_FIRST && t <= DREQ_REG_LAST => {
                GdbRequestValue::GdbRequestRegisterValue(Default::default())
            }
            DREQ_RESTART => GdbRequestValue::GdbRequestRestart(Default::default()),
            DREQ_CONT => GdbRequestValue::GdbRequestCont(Default::default()),
            DREQ_RD_CMD => GdbRequestValue::GdbRequestText(Default::default()),
            DREQ_TLS => GdbRequestValue::GdbRequestTls(Default::default()),
            DREQ_QSYMBOL => GdbRequestValue::GdbRequestSymbol(Default::default()),
            DREQ_FILE_SETFS => GdbRequestValue::GdbRequestFileSetfs(Default::default()),
            DREQ_FILE_OPEN => GdbRequestValue::GdbRequestFileOpen(Default::default()),
            DREQ_FILE_PREAD => GdbRequestValue::GdbRequestFilePread(Default::default()),
            DREQ_FILE_CLOSE => GdbRequestValue::GdbRequestFileClose(Default::default()),
            _ => panic!("Unknown DREQ: {}", type_),
        };

        GdbRequest {
            type_,
            value,
            target: GdbThreadId::ANY,
            suppress_debugger_stop: false,
        }
    }

    /// Return nonzero if this requires that program execution be resumed in some way.
    pub fn is_resume_request(&self) -> bool {
        self.type_ == DREQ_CONT
    }

    pub fn mem(&self) -> &gdb_request::Mem {
        match &self.value {
            GdbRequestValue::GdbRequestMem(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn watch(&self) -> &gdb_request::Watch {
        match &self.value {
            GdbRequestValue::GdbRequestWatch(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn restart(&self) -> &gdb_request::Restart {
        match &self.value {
            GdbRequestValue::GdbRequestRestart(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn reg(&self) -> &GdbRegisterValue {
        match &self.value {
            GdbRequestValue::GdbRequestRegisterValue(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn cont(&self) -> &gdb_request::Cont {
        match &self.value {
            GdbRequestValue::GdbRequestCont(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn text(&self) -> &OsStr {
        match &self.value {
            GdbRequestValue::GdbRequestText(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn tls(&self) -> &gdb_request::Tls {
        match &self.value {
            GdbRequestValue::GdbRequestTls(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn sym(&self) -> &gdb_request::Symbol {
        match &self.value {
            GdbRequestValue::GdbRequestSymbol(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn file_setfs(&self) -> &gdb_request::FileSetfs {
        match &self.value {
            GdbRequestValue::GdbRequestFileSetfs(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn file_open(&self) -> &gdb_request::FileOpen {
        match &self.value {
            GdbRequestValue::GdbRequestFileOpen(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn file_pread(&self) -> &gdb_request::FilePread {
        match &self.value {
            GdbRequestValue::GdbRequestFilePread(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }

    pub fn file_close(&self) -> &gdb_request::FileClose {
        match &self.value {
            GdbRequestValue::GdbRequestFileClose(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn mem_mut(&mut self) -> &mut gdb_request::Mem {
        match &mut self.value {
            GdbRequestValue::GdbRequestMem(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn watch_mut(&mut self) -> &mut gdb_request::Watch {
        match &mut self.value {
            GdbRequestValue::GdbRequestWatch(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn restart_mut(&mut self) -> &mut gdb_request::Restart {
        match &mut self.value {
            GdbRequestValue::GdbRequestRestart(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn reg_mut(&mut self) -> &mut GdbRegisterValue {
        match &mut self.value {
            GdbRequestValue::GdbRequestRegisterValue(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn cont_mut(&mut self) -> &mut gdb_request::Cont {
        match &mut self.value {
            GdbRequestValue::GdbRequestCont(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn text_mut(&mut self) -> &mut OsString {
        match &mut self.value {
            GdbRequestValue::GdbRequestText(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn tls_mut(&mut self) -> &mut gdb_request::Tls {
        match &mut self.value {
            GdbRequestValue::GdbRequestTls(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn sym_mut(&mut self) -> &mut gdb_request::Symbol {
        match &mut self.value {
            GdbRequestValue::GdbRequestSymbol(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn file_setfs_mut(&mut self) -> &mut gdb_request::FileSetfs {
        match &mut self.value {
            GdbRequestValue::GdbRequestFileSetfs(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn file_open_mut(&mut self) -> &mut gdb_request::FileOpen {
        match &mut self.value {
            GdbRequestValue::GdbRequestFileOpen(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn file_pread_mut(&mut self) -> &mut gdb_request::FilePread {
        match &mut self.value {
            GdbRequestValue::GdbRequestFilePread(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
    pub fn file_close_mut(&mut self) -> &mut gdb_request::FileClose {
        match &mut self.value {
            GdbRequestValue::GdbRequestFileClose(v) => v,
            _ => panic!(
                "Unexpected GdbRequestValue enum variant. GdbRequestType was: {}",
                self.type_
            ),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum GdbRestartType {
    RestartFromPrevious,
    RestartFromEvent,
    RestartFromCheckpoint,
}

impl Default for GdbRestartType {
    fn default() -> Self {
        // Arbitrary
        GdbRestartType::RestartFromPrevious
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum GdbActionType {
    ActionContinue,
    ActionStep,
}

#[derive(Clone)]
pub struct GdbContAction {
    pub type_: GdbActionType,
    pub target: GdbThreadId,
    /// rr allows a 0 signal. We represent that by Option<Sig> where None becomes the 0 signal
    pub maybe_signal_to_deliver: Option<Sig>,
}

impl GdbContAction {
    pub fn new(
        maybe_type: Option<GdbActionType>,
        maybe_target: Option<GdbThreadId>,
        maybe_signal_to_deliver: Option<Sig>,
    ) -> GdbContAction {
        GdbContAction {
            type_: maybe_type.unwrap_or(GdbActionType::ActionContinue),
            target: maybe_target.unwrap_or(GdbThreadId::ANY),
            maybe_signal_to_deliver,
        }
    }
}

pub mod gdb_request {
    use super::{GdbContAction, GdbRestartType};
    use crate::{
        remote_ptr::{RemotePtr, Void},
        replay_timeline::RunDirection,
    };
    use libc::pid_t;
    use std::ffi::OsString;

    #[derive(Default, Clone)]
    pub struct Mem {
        pub addr: usize,
        pub len: usize,
        /// For SET_MEM requests, the |len| raw bytes that are to be written.
        /// For SEARCH_MEM requests, the bytes to search for.
        pub data: Vec<u8>,
    }

    #[derive(Default, Clone)]
    pub struct Watch {
        pub addr: usize,
        pub kind: i32,
        pub conditions: Vec<Vec<u8>>,
    }

    #[derive(Default, Clone)]
    pub struct Restart {
        pub param: i32,
        pub param_str: OsString,
        pub type_: GdbRestartType,
    }

    #[derive(Default, Clone)]
    pub struct Cont {
        pub run_direction: RunDirection,
        pub actions: Vec<GdbContAction>,
    }

    #[derive(Default, Clone)]
    pub struct Tls {
        pub offset: usize,
        pub load_module: RemotePtr<Void>,
    }

    #[derive(Default, Clone)]
    pub struct Symbol {
        pub has_address: bool,
        pub address: RemotePtr<Void>,
        pub name: OsString,
    }

    #[derive(Default, Clone)]
    pub struct FileSetfs {
        pub pid: pid_t,
    }

    #[derive(Default, Clone)]
    pub struct FileOpen {
        pub file_name: OsString,
        /// In system format, not gdb's format
        pub flags: i32,
        pub mode: i32,
    }

    #[derive(Default, Clone)]
    pub struct FilePread {
        pub fd: i32,
        pub size: usize,
        pub offset: u64,
    }

    #[derive(Default, Clone)]
    pub struct FileClose {
        pub fd: i32,
    }
}

#[derive(Copy, Clone)]
pub struct GdbConnectionFeatures {
    reverse_execution: bool,
}

impl Default for GdbConnectionFeatures {
    fn default() -> Self {
        Self {
            // This is _not_ an arbitrary choice
            reverse_execution: true,
        }
    }
}

/// This struct wraps up the state of the gdb protocol, so that we can
/// offer a (mostly) stateless interface to clients.
pub struct GdbConnection {
    /// Current request to be processed.
    req: GdbRequest,
    /// Thread to be resumed.
    resume_thread: GdbThreadId,
    /// Thread for get/set requests.
    query_thread: GdbThreadId,
    /// gdb and rd don't work well together in multi-process and
    /// multi-exe-image debugging scenarios, so we pretend only
    /// this thread group exists when interfacing with gdb
    tgid: pid_t,
    cpu_features_: u32,
    /// true when "no-ack mode" enabled, in which we don't have
    /// to send ack packets back to gdb.  This is a huge perf win.
    no_ack: bool,
    sock_fd: ScopedFd,
    /// buffered input from gdb
    inbuf: Vec<u8>,
    /// index of '#' character
    packetend: usize,
    /// buffered output from gdb
    outbuf: Vec<u8>,
    features_: GdbConnectionFeatures,
    connection_alive_: bool,
    /// client supports multiprocess extension
    multiprocess_supported_: bool,
}

impl GdbConnection {
    pub fn new(tgid: pid_t, features: GdbConnectionFeatures) -> GdbConnection {
        GdbConnection {
            tgid,
            cpu_features_: 0,
            no_ack: false,
            features_: features,
            connection_alive_: true,
            // Implied settings
            req: Default::default(),
            resume_thread: Default::default(),
            query_thread: Default::default(),
            sock_fd: Default::default(),
            inbuf: Default::default(),
            packetend: Default::default(),
            outbuf: Default::default(),
            multiprocess_supported_: Default::default(),
        }
    }

    /// Call this when the target of `req` is needed to fulfill the
    /// request, but the target is dead.  This situation is a symptom of a
    /// gdb or rd bug.
    pub fn notify_no_such_thread(&mut self, req: &GdbRequest) {
        debug_assert_eq!(
            unsafe {
                // @TODO Not sure about this approach!
                libc::memcmp(
                    &raw const self.req as _,
                    req as *const GdbRequest as _,
                    size_of_val(&req),
                )
            },
            0
        );

        // '10' is the errno ECHILD.  We use it as a magic code to
        // notify the user that the thread that was the target of this
        // request has died, and either gdb didn't notice that, or rr
        // didn't notify gdb.  Either way, the user should restart
        // their debugging session. */
        log!(
            LogError,
            "Targeted thread no longer exists; this is the result of either a gdb or\n\
                rd bug.  Please restart your debugging session and avoid doing whatever\n\
                triggered this bug."
        );
        self.write_packet_bytes(b"E10");
        self.consume_request();
    }

    /// Finish a DREQ_RESTART request.  Should be invoked after replay
    /// restarts and prior GdbConnection has been restored.
    pub fn notify_restart(&mut self) {
        debug_assert_eq!(DREQ_RESTART, self.req.type_);

        // These threads may not exist at the first trace-stop after
        // restart.  The gdb client should reset this state, but help
        // it out just in case.
        self.resume_thread = GdbThreadId::ANY;
        self.query_thread = GdbThreadId::ANY;

        self.req = GdbRequest::new(None);
    }

    /// Return the current request made by the debugger host, that needs to
    /// be satisfied.  This function will block until either there's a
    /// debugger host request that needs a response, or until a request is
    /// made to resume execution of the target.  In the latter case,
    /// calling this function multiple times will return an appropriate
    /// resume request each time (see above).
    ///
    /// The target should peek at the debugger request in between execution
    /// steps.  A new request may need to be serviced.
    ///
    /// DIFF NOTE: In rr this returns a GdbRequest, here we return a reference
    pub fn get_request(&mut self) -> &GdbRequest {
        if DREQ_RESTART == self.req.type_ {
            log!(LogDebug, "consuming RESTART request");
            self.notify_restart();
            // gdb wants to be notified with a stop packet when
            // the process "relaunches".  In rd's case, the
            // traceee may be very far away from process creation,
            // but that's OK.
            self.req = GdbRequest::new(Some(DREQ_GET_STOP_REASON));
            self.req.target = self.query_thread;
            return &self.req;
        }

        // Can't ask for the next request until you've satisfied the
        // current one, for requests that need an immediate
        // response.
        // DIFF NOTE: This is gated behind a #ifdef DEBUG in rr.
        debug_assert_eq!(request_needs_immediate_response(&self.req), false);

        if !self.sniff_packet() && self.req.is_resume_request() {
            // There's no new request data available and gdb has
            // already asked us to resume.  OK, do that (or keep
            // doing that) now.
            return &self.req;
        }

        loop {
            // There's either new request data, or we have nothing
            // to do.  Either way, block until we read a complete
            // packet from gdb.
            self.read_packet();

            if !self.connection_alive_ {
                self.req = GdbRequest::new(Some(DREQ_DETACH));
                return &self.req;
            }

            if self.process_packet() {
                // We couldn't process the packet internally,
                // so the target has to do something.
                return &self.req;
            }
            // The packet we got was "internal", gdb details.
            // Nothing for the target to do yet.  Keep waiting.
        }
    }

    /// Notify the host that this process has exited with `code`.
    /// DIFF NOTE: On rr code is an int
    pub fn notify_exit_code(&mut self, code: u8) {
        debug_assert!(self.req.is_resume_request() || self.req.type_ == DREQ_INTERRUPT);

        let mut buf = Vec::<u8>::new();
        write!(buf, "W{:02x}", code).unwrap();
        self.write_packet_bytes(&buf);

        self.consume_request();
    }

    /// Notify the host that this process has exited from |sig|.
    pub fn notify_exit_signal(&mut self, sig: Sig) {
        debug_assert!(self.req.is_resume_request() || self.req.type_ == DREQ_INTERRUPT);

        let mut buf = Vec::<u8>::new();
        write!(buf, "X{:02x}", sig.as_raw()).unwrap();
        self.write_packet_bytes(&buf);

        self.consume_request();
    }

    /// Notify the host that a resume request has "finished", i.e., the
    /// target has stopped executing for some reason.  `maybe_sig` is the signal
    /// that stopped execution, or `None` if execution stopped otherwise.
    pub fn notify_stop(
        &mut self,
        thread: GdbThreadId,
        maybe_sig: Option<Sig>,
        watch_addr: RemotePtr<u8>,
    ) {
        debug_assert!(self.req.is_resume_request() || self.req.type_ == DREQ_INTERRUPT);

        if self.tgid != thread.pid {
            log!(
                LogDebug,
                "ignoring stop of {} because we're debugging tgid {}",
                thread,
                self.tgid
            );
            // Re-use the existing continue request to advance to
            // the next stop we're willing to tell gdb about.
            return;
        }
        self.send_stop_reply_packet(thread, maybe_sig, watch_addr);

        // This isn't documented in the gdb remote protocol, but if we
        // don't do this, gdb will sometimes continue to send requests
        // for the previously-stopped thread when it obviously intends
        // to be making requests about the stopped thread.
        // To make things even better, gdb expects different behavior
        // for forward continue/interupt and reverse continue.
        if self.req.is_resume_request()
            && self.req.cont().run_direction == RunDirection::RunBackward
        {
            log!(
                LogDebug,
                "Setting query/resume_thread to ANY after reverse continue"
            );
            self.resume_thread = GdbThreadId::ANY;
            self.query_thread = self.resume_thread;
        } else {
            log!(
                LogDebug,
                "Setting query/resume_thread to {} after forward continue or interrupt",
                thread
            );
            self.resume_thread = thread;
            self.query_thread = self.resume_thread;
        }

        self.consume_request();
    }

    /// Notify the debugger that a restart request failed.
    pub fn notify_restart_failed(&mut self) {
        debug_assert_eq!(DREQ_RESTART, self.req.type_);

        // @TODO: Does gdb knows how to recover from a failed "run" request?
        self.write_packet_bytes(b"E01");

        self.consume_request();
    }

    /// Tell the host that `thread` is the current thread.
    pub fn reply_get_current_thread(&mut self, thread: GdbThreadId) {
        debug_assert_eq!(DREQ_GET_CURRENT_THREAD, self.req.type_);

        let mut buf = Vec::<u8>::new();
        if self.multiprocess_supported_ {
            write!(buf, "QCp{:02x}.{:02x}", thread.pid, thread.tid).unwrap();
        } else {
            write!(buf, "QC{:02x}", thread.tid).unwrap();
        }
        self.write_packet_bytes(&buf);

        self.consume_request();
    }

    /// Reply with the target thread's |auxv| pairs. |auxv.empty()|
    /// if there was an error reading the auxiliary vector.
    pub fn reply_get_auxv(&mut self, auxv: &[u8]) {
        debug_assert_eq!(DREQ_GET_AUXV, self.req.type_);

        if !auxv.is_empty() {
            self.write_binary_packet(b"l", auxv);
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// Reply with the target thread's executable file name
    pub fn reply_get_exec_file(&mut self, exec_file: &OsStr) {
        debug_assert_eq!(DREQ_GET_EXEC_FILE, self.req.type_);

        if !exec_file.is_empty() {
            self.write_binary_packet(b"l", exec_file.as_bytes());
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// |alive| is true if the requested thread is alive, false if dead.
    pub fn reply_get_is_thread_alive(&mut self, alive: bool) {
        debug_assert_eq!(DREQ_GET_IS_THREAD_ALIVE, self.req.type_);

        if alive {
            self.write_packet_bytes(b"OK");
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// |info| is a string containing data about the request target that
    /// might be relevant to the debugger user.
    pub fn reply_get_thread_extra_info(&mut self, info: &OsStr) {
        debug_assert_eq!(DREQ_GET_THREAD_EXTRA_INFO, self.req.type_);

        log!(LogDebug, "thread extra info: {:?}", info);
        self.write_hex_bytes_packet(info.as_bytes());

        self.consume_request();
    }

    /// |ok| is true if req->target can be selected, false otherwise.
    pub fn reply_select_thread(&mut self, ok: bool) {
        debug_assert!(
            DREQ_SET_CONTINUE_THREAD == self.req.type_ || DREQ_SET_QUERY_THREAD == self.req.type_
        );

        if ok && DREQ_SET_CONTINUE_THREAD == self.req.type_ {
            self.resume_thread = self.req.target;
        } else if ok && DREQ_SET_QUERY_THREAD == self.req.type_ {
            self.query_thread = self.req.target;
        }

        if ok {
            self.write_packet_bytes(b"OK");
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// The first |mem.size()| bytes of the request were read into |mem|.
    /// |mem.size()| must be less than or equal to the length of the request.
    pub fn reply_get_mem(&mut self, mem: &[u8]) {
        debug_assert_eq!(DREQ_GET_MEM, self.req.type_);
        debug_assert!(mem.len() <= self.req.mem().len);

        if self.req.mem().len > 0 && mem.len() == 0 {
            self.write_packet_bytes(b"E01");
        } else {
            self.write_hex_bytes_packet(mem);
        }

        self.consume_request();
    }

    /// |ok| is true if a SET_MEM request succeeded, false otherwise.  This
    /// function *must* be called whenever a SET_MEM request is made,
    /// regardless of success/failure or special interpretation.
    pub fn reply_set_mem(&mut self, ok: bool) {
        debug_assert_eq!(DREQ_SET_MEM, self.req.type_);

        if ok {
            self.write_packet_bytes(b"OK");
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// Reply to the DREQ_SEARCH_MEM request.
    /// |found| is true if we found the searched-for bytes starting at address |addr|.
    pub fn reply_search_mem(&mut self, found: bool, addr: RemotePtr<Void>) {
        debug_assert_eq!(DREQ_SEARCH_MEM, self.req.type_);

        if found {
            let mut buf = Vec::<u8>::new();
            write!(buf, "1,{:x}", addr.as_usize()).unwrap();
            self.write_packet_bytes(&buf);
        } else {
            self.write_packet_bytes(b"0");
        }

        self.consume_request();
    }

    /// Reply to the DREQ_GET_OFFSETS request.
    pub fn reply_get_offsets(&mut self /* TODO*/) {
        debug_assert_eq!(DREQ_GET_OFFSETS, self.req.type_);

        // XXX FIXME TODO
        self.write_packet_bytes(b"");

        self.consume_request();
    }

    /// Send |value| back to the debugger host.  |value| may be undefined.
    pub fn reply_get_reg(&mut self, reg: &GdbRegisterValue) {
        let mut buf = Vec::<u8>::new();

        debug_assert_eq!(DREQ_GET_REG, self.req.type_);

        print_reg_value(&reg, &mut buf);
        self.write_packet_bytes(&buf);

        self.consume_request();
    }

    /// Send |file| back to the debugger host.  |file| may contain
    /// undefined register values.
    pub fn reply_get_regs(&mut self, file: &[GdbRegisterValue]) {
        let mut buf = Vec::<u8>::new();

        debug_assert_eq!(DREQ_GET_REGS, self.req.type_);

        for reg in file {
            print_reg_value(reg, &mut buf);
        }
        self.write_packet_bytes(&buf);

        self.consume_request();
    }

    /// Pass |ok = true| iff the requested register was successfully set.
    pub fn reply_set_reg(&mut self, ok: bool) {
        debug_assert_eq!(DREQ_SET_REG, self.req.type_);

        // TODO: what happens if we're forced to reply to a
        // set-register request with |ok = false|, leading us to
        // pretend not to understand the packet?  If, later, an
        // experimental session needs the set-register request will it
        // not be sent?
        //
        // We can't reply with an error packet here because gdb thinks
        // that failed set-register requests are catastrophic.
        if ok {
            self.write_packet_bytes(b"OK")
        } else {
            self.write_packet_bytes(&[])
        }

        self.consume_request();
    }

    /// Reply to the DREQ_GET_STOP_REASON request.
    pub fn reply_get_stop_reason(&mut self, which: GdbThreadId, sig: Sig) {
        debug_assert_eq!(DREQ_GET_STOP_REASON, self.req.type_);

        self.send_stop_reply_packet(which, Some(sig), RemotePtr::null());

        self.consume_request();
    }

    /// `threads` contains the list of live threads.
    pub fn reply_get_thread_list(&mut self, threads: &[GdbThreadId]) {
        debug_assert_eq!(DREQ_GET_THREAD_LIST, self.req.type_);
        if threads.is_empty() {
            self.write_packet_bytes(b"l");
        } else {
            let mut buf = Vec::<u8>::new();
            buf.push(b'm');
            for &t in threads {
                if self.tgid != t.pid {
                    continue;
                }
                if self.multiprocess_supported_ {
                    write!(buf, "p{:02x}.{:02x},", t.pid, t.tid).unwrap();
                } else {
                    write!(buf, "{:02x},", t.tid).unwrap();
                }
            }
            // Omit the trailing `,`
            self.write_packet_bytes(&buf[..buf.len() - 1]);
        }

        self.consume_request();
    }

    /// |ok| is true if the request was successfully applied, false if not.
    pub fn reply_watchpoint_request(&mut self, ok: bool) {
        debug_assert!(DREQ_WATCH_FIRST <= self.req.type_ && self.req.type_ <= DREQ_WATCH_LAST);
        if ok {
            self.write_packet_bytes(b"OK");
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// DREQ_DETACH was processed.
    ///
    /// There's no functional reason to reply to the detach request.
    /// However, some versions of gdb expect a response and time out
    /// awaiting it, wasting developer time.
    pub fn reply_detach(&mut self) {
        debug_assert!(DREQ_DETACH <= self.req.type_);

        self.write_packet_bytes(b"OK");

        self.consume_request();
    }

    /// Pass the siginfo_t and its size (as requested by the debugger) in
    /// `si_bytes` if successfully read.  Otherwise pass si_bytes = nullptr.
    pub fn reply_read_siginfo(&mut self, si_bytes: &[u8]) {
        debug_assert_eq!(DREQ_READ_SIGINFO, self.req.type_);

        if si_bytes.is_empty() {
            self.write_packet_bytes(b"E01");
        } else {
            self.write_binary_packet(b"l", si_bytes);
        }

        self.consume_request();
    }

    /// Not yet implemented, but call this after a WRITE_SIGINFO request
    /// anyway.
    pub fn reply_write_siginfo(&mut self /* TODO*/) {
        debug_assert_eq!(DREQ_WRITE_SIGINFO, self.req.type_);

        self.write_packet_bytes(b"E01");

        self.consume_request();
    }

    /// Send a manual text response to a rr cmd (maintenance) packet.
    pub fn reply_rd_cmd(&mut self, text: &str) {
        debug_assert_eq!(DREQ_RD_CMD, self.req.type_);

        self.write_packet_bytes(text.as_bytes());

        self.consume_request();
    }

    /// Send a qSymbol response to gdb, requesting the address of the
    /// symbol |name|.
    pub fn send_qsymbol(&mut self, name: &str) {
        debug_assert_eq!(DREQ_QSYMBOL, self.req.type_);

        self.write_hex_bytes_packet_with_prefix(b"qSymbol:", name.as_bytes());

        self.consume_request();
    }

    /// The "all done" response to a qSymbol packet from gdb.
    pub fn qsymbols_finished(&mut self) {
        debug_assert_eq!(DREQ_QSYMBOL, self.req.type_);

        self.write_packet_bytes(b"OK");

        self.consume_request();
    }

    /// Respond to a qGetTLSAddr packet.  If |ok| is true, then respond
    /// with |address|.  If |ok| is false, respond with an error.
    pub fn reply_tls_addr(&mut self, ok: bool, addr: RemotePtr<Void>) {
        debug_assert_eq!(DREQ_TLS, self.req.type_);

        if ok {
            let mut buf = Vec::<u8>::new();
            write!(buf, "{:x};", addr.as_usize()).unwrap();
            self.write_packet_bytes(&buf);
        } else {
            self.write_packet_bytes(b"E01");
        }

        self.consume_request();
    }

    /// Respond to a vFile:setfs
    pub fn reply_setfs(&mut self, err: i32) {
        debug_assert_eq!(DREQ_FILE_SETFS, self.req.type_);
        if err != 0 {
            self.send_file_error_reply(err);
        } else {
            self.write_packet_bytes(b"F0");
        }

        self.consume_request();
    }

    /// Respond to a vFile:open
    pub fn reply_open(&mut self, fd: i32, err: i32) {
        debug_assert_eq!(DREQ_FILE_OPEN, self.req.type_);
        if err != 0 {
            self.send_file_error_reply(err);
        } else {
            let mut buf = Vec::<u8>::new();
            write!(buf, "F{:x};", fd).unwrap();
            self.write_packet_bytes(&buf);
        }

        self.consume_request();
    }

    /// Respond to a vFile:pread
    pub fn reply_pread(&mut self, bytes: &[u8], err: i32) {
        debug_assert_eq!(DREQ_FILE_PREAD, self.req.type_);
        if err != 0 {
            self.send_file_error_reply(err);
        } else {
            let mut buf = Vec::<u8>::new();
            write!(buf, "F{:x};", bytes.len()).unwrap();
            self.write_binary_packet(&buf, bytes);
        }

        self.consume_request();
    }

    /// Respond to a vFile:close
    pub fn reply_close(&mut self, err: i32) {
        debug_assert_eq!(DREQ_FILE_CLOSE, self.req.type_);
        if err != 0 {
            self.send_file_error_reply(err);
        } else {
            self.write_packet_bytes(b"F0");
        }

        self.consume_request();
    }

    /// Create a checkpoint of the given Session with the given id. Delete the
    /// existing checkpoint with that id if there is one.
    ///
    /// DIFF NOTE: The checkpoint id is signed in rr
    /// DIFF NOTE: In rr we pass in a ReplaySession shared pointer
    fn created_checkpoint(_checkpoint: SessionSharedPtr, _checkpoint_id: u32) {
        unimplemented!()
    }

    /// Delete the checkpoint with the given id. Silently fail if the checkpoint
    /// does not exist.
    ///
    /// DIFF NOTE: The checkpoint id is signed in rr
    pub fn delete_checkpoint(_checkpoint_id: u32) {
        unimplemented!()
    }

    /// Get the checkpoint with the given id. Return null if not found.
    pub fn get_checkpoint(_checkpoint_id: u32) -> SessionSharedPtr {
        unimplemented!()
    }

    /// Return true if there's a new packet to be read/process (whether
    /// incomplete or not), and false if there isn't one.
    pub fn sniff_packet(&mut self) -> bool {
        if self.skip_to_packet_start() {
            // We've already seen a (possibly partial) packet.
            return true;
        }
        parser_assert!(self.inbuf.is_empty());
        return poll_incoming(&self.sock_fd, 0 /*don't wait*/);
    }

    pub fn features(&self) -> GdbConnectionFeatures {
        self.features_
    }

    pub fn set_cpu_features(&mut self, features: u32) {
        self.cpu_features_ = features
    }

    pub fn cpu_features(&self) -> u32 {
        self.cpu_features_
    }

    /// Wait for a debugger client to connect to |dbg|'s socket.  Blocks
    /// indefinitely.
    pub fn await_debugger(&mut self, listen_fd: &ScopedFd) {
        self.sock_fd = ScopedFd::from_raw(accept(listen_fd.as_raw()).unwrap());
        // We might restart this debugging session, so don't set the
        // socket fd CLOEXEC.
    }

    ///  Returns false if the connection has been closed
    pub fn is_connection_alive(&self) -> bool {
        self.connection_alive_
    }

    /// read() incoming data exactly one time, successfully.  May block.
    fn read_data_once(&mut self) {
        // Wait until there's data, instead of busy-looping on EAGAIN.
        poll_incoming(&self.sock_fd, -1 /* wait forever */);
        let mut buf = [0u8; 4096];
        let result = unistd::read(self.sock_fd.as_raw(), &mut buf);
        match result {
            Ok(0) | Err(_) => {
                log!(
                    LogInfo,
                    "Could not read data from gdb socket, marking connection as closed"
                );
                self.connection_alive_ = false;
            }
            Ok(nread) => {
                self.inbuf.extend_from_slice(&buf[0..nread]);
            }
        }
    }

    /// Send all pending output to gdb.  May block.
    fn write_flush(&mut self) {
        let mut write_index: usize = 0;

        log!(
            LogDebug,
            "write_flush: {:?}",
            OsStr::from_bytes(&self.outbuf)
        );

        while write_index < self.outbuf.len() {
            poll_outgoing(&self.sock_fd, -1 /*wait forever*/);
            let result = unistd::write(self.sock_fd.as_raw(), &mut self.outbuf[write_index..]);
            match result {
                Err(_) => {
                    log!(
                        LogInfo,
                        "Could not write data to gdb socket, marking connection as closed",
                    );
                    self.connection_alive_ = false;
                    self.outbuf.clear();
                    return;
                }
                Ok(nwritten) => {
                    write_index += nwritten;
                }
            }
        }

        self.outbuf.clear();
    }

    fn write_data_raw(&mut self, data: &[u8]) {
        self.outbuf.extend_from_slice(data);
    }

    fn write_hex(&mut self, hex: usize) {
        let mut buf: Vec<u8> = Vec::new();

        write!(buf, "{:02x}", hex).unwrap();
        self.write_data_raw(&buf);
    }

    fn write_packet_bytes(&mut self, data: &[u8]) {
        let mut checksum: u8 = 0;

        self.write_data_raw(b"$");
        for &b in data {
            checksum = checksum.overflowing_add(b).0;
        }
        self.write_data_raw(data);
        self.write_data_raw(b"#");
        self.write_hex(checksum as usize);
    }

    /// DIFF NOTE: prefix is a null terminated c-string in rr. Here its just a slice.
    fn write_binary_packet(&mut self, pfx: &[u8], data: &[u8]) {
        let pfx_num_chars = pfx.len();
        let num_bytes = data.len();
        let mut buf = Vec::<u8>::with_capacity(2 * num_bytes + pfx_num_chars);

        buf.extend_from_slice(pfx);
        for &b in data {
            match b {
                b'#' | b'$' | b'}' | b'*' => {
                    buf.push(b'}');
                    buf.push(b ^ 0x20);
                }
                _ => {
                    buf.push(b);
                }
            }
        }

        log!(
            LogDebug,
            " ***** NOTE: writing binary data, upcoming debug output may be truncated"
        );

        self.write_packet_bytes(&buf);
    }

    /// DIFF NOTE: prefix is a null terminated c-string in rr. here its just a slice.
    fn write_hex_bytes_packet_with_prefix(&mut self, prefix: &[u8], data: &[u8]) {
        let mut buf = Vec::<u8>::with_capacity(prefix.len() + 2 * data.len());
        buf.extend_from_slice(prefix);
        for &b in data {
            write!(buf, "{:02x}", b).unwrap();
        }
        self.write_packet_bytes(&buf);
    }

    fn write_hex_bytes_packet(&mut self, data: &[u8]) {
        self.write_hex_bytes_packet_with_prefix(&[], data)
    }

    // DIFF NOTE: `offset` and `len` are u64 in rr
    fn write_xfer_response(&mut self, data: &[u8], offset: usize, len: usize) {
        let size = data.len();
        if offset > size {
            self.write_packet_bytes(b"E01");
        } else if offset == size {
            self.write_packet_bytes(b"l");
        } else if offset + len < size {
            self.write_binary_packet(b"m", &data[offset..offset + len]);
        } else {
            self.write_binary_packet(b"l", &data[offset..]);
        }
    }

    /// Consume bytes in the input buffer until start-of-packet ('$') or
    /// the interrupt character is seen.  Does not block.  Return true if
    /// seen, false if not.
    fn skip_to_packet_start(&mut self) -> bool {
        let mut maybe_end = None;
        // Can we make this more efficient?
        // XXX we want memcspn() here
        for i in 0..self.inbuf.len() {
            if self.inbuf[i] == b'$' || self.inbuf[i] == INTERRUPT_CHAR {
                maybe_end = Some(i);
                break;
            }
        }
        match maybe_end {
            None => {
                // Discard all read bytes, which we don't care about
                self.inbuf.clear();
                return false;
            }
            Some(end) => {
                // Discard bytes up to start-of-packet
                self.inbuf.drain(..end);
            }
        }

        parser_assert!(1 <= self.inbuf.len());
        parser_assert!(b'$' == self.inbuf[0] || INTERRUPT_CHAR == self.inbuf[0]);

        true
    }

    /// Block until the sequence of bytes
    ///
    ///    "[^$]*\$[^#]*#.*"
    ///
    /// has been read from the client fd.  This is one (or more) gdb
    /// packet(s).
    fn read_packet(&self) {
        unimplemented!()
    }

    /// Return true if we need to do something in a debugger request,
    /// false if we already handled the packet internally.
    fn xfer(_name: &OsStr, _args: &[&OsStr]) -> bool {
        unimplemented!()
    }

    /// Return true if we need to do something in a debugger request,
    /// false if we already handled the packet internally.
    fn query(_payload: &[u8]) -> bool {
        unimplemented!()
    }

    /// Return true if we need to do something in a debugger request,
    /// false if we already handled the packet internally.
    fn set_var(_payload: &[u8]) -> bool {
        unimplemented!()
    }

    /// Return true if we need to do something in a debugger request,
    /// false if we already handled the packet internally.
    fn process_vpacket(_payload: &[u8]) -> bool {
        unimplemented!()
    }

    /// Return true if we need to do something in a debugger request,
    /// false if we already handled the packet internally.
    fn process_bpacket(&mut self, payload: &[u8]) -> bool {
        if payload == b"c" {
            self.req = GdbRequest::new(Some(DREQ_CONT));
            self.req.cont_mut().run_direction = RunDirection::RunBackward;
            self.req.cont_mut().actions.push(GdbContAction::new(
                Some(GdbActionType::ActionContinue),
                Some(self.resume_thread),
                None,
            ));
            return true;
        } else if payload == b"s" {
            self.req = GdbRequest::new(Some(DREQ_CONT));
            self.req.cont_mut().run_direction = RunDirection::RunBackward;
            self.req.cont_mut().actions.push(GdbContAction::new(
                Some(GdbActionType::ActionStep),
                Some(self.resume_thread),
                None,
            ));
            return true;
        } else {
            self.write_packet_bytes(&[]);
            log!(
                LogInfo,
                "Unhandled gdb bpacket: b{:?}",
                OsStr::from_bytes(payload)
            );
            return false;
        }
    }

    /// Return true if we need to do something in a debugger request,
    /// false if we already handled the packet internally.
    fn process_packet(&self) -> bool {
        unimplemented!()
    }

    fn consume_request(&mut self) {
        self.req = GdbRequest::new(None);
        self.write_flush()
    }

    fn send_stop_reply_packet(
        &mut self,
        thread: GdbThreadId,
        maybe_sig: Option<Sig>,
        watch_addr: RemotePtr<u8>,
    ) {
        let mut buf = Vec::<u8>::new();
        if self.multiprocess_supported_ {
            write!(
                buf,
                "T{:02x}thread:p{:02x}.{:02x};",
                to_gdb_signum(maybe_sig),
                thread.pid,
                thread.tid,
            )
            .unwrap();
        } else {
            write!(
                buf,
                "T{:02x}thread:{:02x};",
                to_gdb_signum(maybe_sig),
                thread.tid,
            )
            .unwrap();
        }
        if !watch_addr.is_null() {
            write!(buf, "watch:{};", watch_addr.as_usize()).unwrap();
        }
        self.write_packet_bytes(&buf);
    }

    fn send_file_error_reply(&mut self, system_errno: i32) {
        let gdb_err;
        match system_errno {
            libc::EPERM => {
                gdb_err = 1;
            }
            libc::ENOENT => {
                gdb_err = 2;
            }
            libc::EINTR => {
                gdb_err = 4;
            }
            libc::EBADF => {
                gdb_err = 9;
            }
            libc::EACCES => {
                gdb_err = 13;
            }
            libc::EFAULT => {
                gdb_err = 14;
            }
            libc::EBUSY => {
                gdb_err = 16;
            }
            libc::EEXIST => {
                gdb_err = 17;
            }
            libc::ENODEV => {
                gdb_err = 19;
            }
            libc::ENOTDIR => {
                gdb_err = 20;
            }
            libc::EISDIR => {
                gdb_err = 21;
            }
            libc::EINVAL => {
                gdb_err = 22;
            }
            libc::ENFILE => {
                gdb_err = 23;
            }
            libc::EMFILE => {
                gdb_err = 24;
            }
            libc::EFBIG => {
                gdb_err = 27;
            }
            libc::ENOSPC => {
                gdb_err = 28;
            }
            libc::ESPIPE => {
                gdb_err = 29;
            }
            libc::EROFS => {
                gdb_err = 30;
            }
            libc::ENAMETOOLONG => {
                gdb_err = 91;
            }
            _ => {
                gdb_err = 9999;
            }
        };
        let mut buf = Vec::<u8>::new();
        write!(buf, "F-01,{:x}", gdb_err).unwrap();
        self.write_packet_bytes(&buf);
    }
}

fn poll_incoming(sock_fd: &ScopedFd, timeout_ms: i32) -> bool {
    poll_socket(
        sock_fd,
        PollFlags::POLLIN, /* TODO: |POLLERR */
        timeout_ms,
    )
}

fn poll_outgoing(sock_fd: &ScopedFd, timeout_ms: i32) -> bool {
    poll_socket(
        sock_fd,
        PollFlags::POLLOUT, /* TODO: |POLLERR */
        timeout_ms,
    )
}

/// Poll for data to or from gdb, waiting `timeoutMs`.  0 means "don't
/// wait", and -1 means "wait forever".  Return true if data is ready.
fn poll_socket(sock_fd: &ScopedFd, events: PollFlags, timeout_ms: i32) -> bool {
    let mut pfds = [PollFd::new(sock_fd.as_raw(), events)];

    match poll(&mut pfds, timeout_ms) {
        Ok(ret) if ret > 0 => return true,
        Err(Error::Sys(err)) if err != Errno::EINTR => log!(LogInfo, "gdb socket has been closed"),
        _ => (),
    }

    false
}

fn decode_ascii_encoded_hex_str(encoded: &[u8]) -> String {
    let enc_len = encoded.len();
    parser_assert_eq!(enc_len % 2, 0);
    let mut decoded_str = String::new();
    for i in 0..enc_len {
        let enc_byte_str = std::str::from_utf8(&encoded[2 * i..2 * i + 2]).unwrap();
        let c_u8 = u8::from_str_radix(enc_byte_str, 16).unwrap();
        parser_assert!(c_u8 < 128);
        let c: char = c_u8.into();
        decoded_str.push(c);
    }

    decoded_str
}

/// Format `value` into `buf` in the manner gdb expects.
fn print_reg_value(reg: &GdbRegisterValue, buf: &mut Vec<u8>) {
    parser_assert!(reg.size <= GdbRegisterValue::MAX_SIZE);
    if reg.defined {
        // gdb wants the register value in native endianness.
        // reg.value read in native endianness is exactly that.
        match reg.value {
            GdbRegisterValueData::Value(v) => {
                for i in 0..reg.size {
                    write!(buf, "{:02x}", v[i]).unwrap();
                }
            }
            GdbRegisterValueData::Value1(v) => {
                write!(buf, "{:02x}", v).unwrap();
            }
            GdbRegisterValueData::Value2(v) => {
                write!(buf, "{:04x}", v).unwrap();
            }
            GdbRegisterValueData::Value4(v) => {
                write!(buf, "{:08x}", v).unwrap();
            }
            GdbRegisterValueData::Value8(v) => {
                write!(buf, "{:016x}", v).unwrap();
            }
        }
    } else {
        for _ in 0..reg.size {
            write!(buf, "xx").unwrap();
        }
    }
}

// Translate linux-x86 |sig| to gdb's internal numbering.  Translation
// made according to gdb/include/gdb/signals.def.
fn to_gdb_signum(maybe_sig: Option<Sig>) -> i32 {
    let sig = match maybe_sig {
        Some(sig) => sig.as_raw(),
        None => return 0,
    };
    match sig {
        0 => {
            return 0;
        }
        libc::SIGHUP => {
            return 1;
        }
        libc::SIGINT => {
            return 2;
        }
        libc::SIGQUIT => {
            return 3;
        }
        libc::SIGILL => {
            return 4;
        }
        libc::SIGTRAP => {
            return 5;
        }
        libc::SIGABRT => {
            // libc::SIGIOT
            return 6;
        }
        libc::SIGBUS => {
            return 10;
        }
        libc::SIGFPE => {
            return 8;
        }
        libc::SIGKILL => {
            return 9;
        }
        libc::SIGUSR1 => {
            return 30;
        }
        libc::SIGSEGV => {
            return 11;
        }
        libc::SIGUSR2 => {
            return 31;
        }
        libc::SIGPIPE => {
            return 13;
        }
        libc::SIGALRM => {
            return 14;
        }
        libc::SIGTERM => {
            return 15;
        }
        // gdb hasn't heard of libc::SIGSTKFLT, so this is
        // arbitrarily made up.  libc::SIGDANGER just sounds cool
        libc::SIGSTKFLT => {
            return 38; // GDB_libc::SIGNAL_DANGER
        }
        /* case libc::SIGCLD */ libc::SIGCHLD => {
            return 20;
        }
        libc::SIGCONT => {
            return 19;
        }
        libc::SIGSTOP => {
            return 17;
        }
        libc::SIGTSTP => {
            return 18;
        }
        libc::SIGTTIN => {
            return 21;
        }
        libc::SIGTTOU => {
            return 22;
        }
        libc::SIGURG => {
            return 16;
        }
        libc::SIGXCPU => {
            return 24;
        }
        libc::SIGXFSZ => {
            return 25;
        }
        libc::SIGVTALRM => {
            return 26;
        }
        libc::SIGPROF => {
            return 27;
        }
        libc::SIGWINCH => {
            return 28;
        }
        /* case libc::SIGPOLL */ libc::SIGIO => {
            return 23;
        }
        libc::SIGPWR => {
            return 32;
        }
        libc::SIGSYS => {
            return 12;
        }
        32 => {
            return 77;
        }
        _ => {
            if 33 <= sig && sig <= 63 {
                // GDB_libc::SIGNAL_REALTIME_33 is numbered 45, hence this offset
                return sig + 12;
            }
            if 64 <= sig && sig <= 127 {
                // GDB_libc::SIGNAL_REALTIME_64 is numbered 78, hence this offset
                return sig + 14;
            }
            log!(LogWarn, "Unknown signal {}", sig);
            return 143; // GDB_libc::SIGNAL_UNKNOWN
        }
    }
}

fn gdb_open_flags_to_system_flags(flags: i64) -> i32 {
    let mut ret: i32;
    match flags & 3 {
        0 => {
            ret = libc::O_RDONLY;
        }
        1 => {
            ret = libc::O_WRONLY;
        }
        2 => {
            ret = libc::O_RDWR;
        }
        _ => {
            parser_assert!(false);
            return 0;
        }
    }
    parser_assert_eq!(0, flags & !(3 | 0x8 | 0x200 | 0x400 | 0x800));
    if flags & 0x8 != 0 {
        ret |= libc::O_APPEND;
    }
    if flags & 0x200 != 0 {
        ret |= libc::O_CREAT;
    }
    if flags & 0x400 != 0 {
        ret |= libc::O_TRUNC;
    }
    if flags & 0x800 != 0 {
        ret |= libc::O_EXCL;
    }

    ret
}

fn request_needs_immediate_response(req: &GdbRequest) -> bool {
    match req.type_ {
        DREQ_NONE | DREQ_CONT => false,
        _ => true,
    }
}
