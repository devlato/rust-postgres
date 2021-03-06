/*!
Rust-Postgres is a pure-Rust frontend for the popular PostgreSQL database. It
exposes a high level interface in the vein of JDBC or Go's `database/sql`
package.

```rust
extern mod postgres = "github.com/sfackler/rust-postgres";
extern mod extra;

use extra::time;
use extra::time::Timespec;

use postgres::{PostgresConnection, PostgresStatement, NoSsl};
use postgres::types::ToSql;

struct Person {
    id: i32,
    name: ~str,
    time_created: Timespec,
    data: Option<~[u8]>
}

fn main() {
    let conn = PostgresConnection::connect("postgres://postgres@localhost",
                                           &NoSsl);

    conn.execute("CREATE TABLE person (
                    id              SERIAL PRIMARY KEY,
                    name            VARCHAR NOT NULL,
                    time_created    TIMESTAMP NOT NULL,
                    data            BYTEA
                  )", []);
    let me = Person {
        id: 0,
        name: ~"Steven",
        time_created: time::get_time(),
        data: None
    };
    conn.execute("INSERT INTO person (name, time_created, data)
                    VALUES ($1, $2, $3)",
                 [&me.name as &ToSql, &me.time_created as &ToSql,
                  &me.data as &ToSql]);

    let stmt = conn.prepare("SELECT id, name, time_created, data FROM person");
    for row in stmt.query([]) {
        let person = Person {
            id: row[1],
            name: row[2],
            time_created: row[3],
            data: row[4]
        };
        println!("Found person {}", person.name);
    }
}
```
 */

#[crate_id="github.com/sfackler/rust-postgres"];
#[crate_type="lib"];
#[doc(html_root_url="http://sfackler.github.io/rust-postgres/doc/")];

#[warn(missing_doc)];

#[feature(macro_rules, struct_variant, globs)];
#[macro_escape];

extern mod extra;
extern mod openssl = "github.com/sfackler/rust-openssl";

use extra::container::Deque;
use extra::hex::ToHex;
use extra::ringbuf::RingBuf;
use extra::url::{UserInfo, Url};
use openssl::crypto::hash::{MD5, Hasher};
use openssl::ssl::{SslStream, SslContext};
use std::cell::RefCell;
use std::io::io_error;
use std::io::buffered::BufferedStream;
use std::io::net;
use std::io::net::ip::{Port, SocketAddr};
use std::io::net::tcp::TcpStream;
use std::task;
use std::hashmap::HashMap;
use std::str;

use self::error::{PostgresDbError,
                  PostgresConnectError,
                  InvalidUrl,
                  DnsError,
                  SocketError,
                  NoSslSupport,
                  SslError,
                  MissingUser,
                  DbError,
                  UnsupportedAuthentication,
                  MissingPassword};
use self::message::{BackendMessage,
                    AuthenticationOk,
                    AuthenticationKerberosV5,
                    AuthenticationCleartextPassword,
                    AuthenticationMD5Password,
                    AuthenticationSCMCredential,
                    AuthenticationGSS,
                    AuthenticationSSPI,
                    BackendKeyData,
                    BindComplete,
                    CommandComplete,
                    DataRow,
                    EmptyQueryResponse,
                    ErrorResponse,
                    NoData,
                    NoticeResponse,
                    NotificationResponse,
                    ParameterDescription,
                    ParameterStatus,
                    ParseComplete,
                    PortalSuspended,
                    ReadyForQuery,
                    RowDescription};
use self::message::{FrontendMessage,
                    Bind,
                    CancelRequest,
                    Close,
                    Describe,
                    Execute,
                    Parse,
                    PasswordMessage,
                    Query,
                    StartupMessage,
                    SslRequest,
                    Sync,
                    Terminate};
use self::message::{RowDescriptionEntry, WriteMessage, ReadMessage};
use self::types::{Oid, PostgresType, ToSql, FromSql, PgUnknownType};

pub mod error;
pub mod pool;
mod message;
pub mod types;

static DEFAULT_PORT: Port = 5432;

/// Trait for types that can handle Postgres notice messages
pub trait PostgresNoticeHandler {
    /// Handle a Postgres notice message
    fn handle(&mut self, notice: PostgresDbError);
}

/// A notice handler which logs at the `info` level.
///
/// This is the default handler used by a `PostgresConnection`.
pub struct DefaultNoticeHandler;

impl PostgresNoticeHandler for DefaultNoticeHandler {
    fn handle(&mut self, notice: PostgresDbError) {
        info!("{}: {}", notice.severity, notice.message);
    }
}

/// An asynchronous notification
pub struct PostgresNotification {
    /// The process ID of the notifying backend process
    pid: i32,
    /// The name of the channel that the notify has been raised on
    channel: ~str,
    /// The "payload" string passed from the notifying process
    payload: ~str,
}

/// An iterator over asynchronous notifications
pub struct PostgresNotificationIterator<'conn> {
    priv conn: &'conn PostgresConnection
}

impl<'conn > Iterator<PostgresNotification> for
        PostgresNotificationIterator<'conn> {
    /// Returns the oldest pending notification or `None` if there are none.
    ///
    /// # Note
    ///
    /// `next` may return `Some` notification after returning `None` if a new
    /// notification was received.
    fn next(&mut self) -> Option<PostgresNotification> {
        self.conn.conn.with_mut(|conn| { conn.notifications.pop_front() })
    }
}

/// Contains information necessary to cancel queries for a session
pub struct PostgresCancelData {
    /// The process ID of the session
    process_id: i32,
    /// The secret key for the session
    secret_key: i32,
}

/// Attempts to cancel an in-progress query.
///
/// The backend provides no information about whether a cancellation attempt
/// was successful or not. An error will only be returned if the driver was
/// unable to connect to the database.
///
/// A `PostgresCancelData` object can be created via
/// `PostgresConnection::cancel_data`. The object can cancel any query made on
/// that connection.
pub fn cancel_query(url: &str, ssl: &SslMode, data: PostgresCancelData)
        -> Result<(), PostgresConnectError> {
    let Url { host, port, .. }: Url = match FromStr::from_str(url) {
        Some(url) => url,
        None => return Err(InvalidUrl)
    };
    let port = match port {
        Some(port) => FromStr::from_str(port).unwrap(),
        None => DEFAULT_PORT
    };

    let mut socket = match initialize_stream(host, port, ssl) {
        Ok(socket) => socket,
        Err(err) => return Err(err)
    };

    socket.write_message(&CancelRequest {
        code: message::CANCEL_CODE,
        process_id: data.process_id,
        secret_key: data.secret_key
    });
    socket.flush();

    Ok(())
}

fn open_socket(host: &str, port: Port)
        -> Result<TcpStream, PostgresConnectError> {
    let addrs = io_error::cond.trap(|_| {}).inside(|| {
        net::get_host_addresses(host)
    });
    let addrs = match addrs {
        Some(addrs) => addrs,
        None => return Err(DnsError)
    };

    for addr in addrs.iter() {
        let socket = io_error::cond.trap(|_| {}).inside(|| {
            TcpStream::connect(SocketAddr { ip: *addr, port: port })
        });
        match socket {
            Some(socket) => return Ok(socket),
            None => {}
        }
    }

    Err(SocketError)
}

fn initialize_stream(host: &str, port: Port, ssl: &SslMode)
        -> Result<InternalStream, PostgresConnectError> {
    let mut socket = match open_socket(host, port) {
        Ok(socket) => socket,
        Err(err) => return Err(err)
    };

    let (ssl_required, ctx) = match ssl {
        &NoSsl => return Ok(Normal(socket)),
        &PreferSsl(ref ctx) => (false, ctx),
        &RequireSsl(ref ctx) => (true, ctx)
    };

    socket.write_message(&SslRequest { code: message::SSL_CODE });
    socket.flush();

    if socket.read_u8() == 'N' as u8 {
        if ssl_required {
            return Err(NoSslSupport);
        } else {
            return Ok(Normal(socket));
        }
    }

    match SslStream::try_new(ctx, socket) {
        Ok(stream) => Ok(Ssl(stream)),
        Err(err) => Err(SslError(err))
    }
}

enum InternalStream {
    Normal(TcpStream),
    Ssl(SslStream<TcpStream>)
}

impl Reader for InternalStream {
    fn read(&mut self, buf: &mut [u8]) -> Option<uint> {
        match *self {
            Normal(ref mut s) => s.read(buf),
            Ssl(ref mut s) => s.read(buf)
        }
    }

    fn eof(&mut self) -> bool {
        match *self {
            Normal(ref mut s) => s.eof(),
            Ssl(ref mut s) => s.eof()
        }
    }
}

impl Writer for InternalStream {
    fn write(&mut self, buf: &[u8]) {
        match *self {
            Normal(ref mut s) => s.write(buf),
            Ssl(ref mut s) => s.write(buf)
        }
    }

    fn flush(&mut self) {
        match *self {
            Normal(ref mut s) => s.flush(),
            Ssl(ref mut s) => s.flush()
        }
    }
}

struct InnerPostgresConnection {
    stream: BufferedStream<InternalStream>,
    next_stmt_id: uint,
    notice_handler: ~PostgresNoticeHandler,
    notifications: RingBuf<PostgresNotification>,
    cancel_data: PostgresCancelData,
    unknown_types: HashMap<Oid, ~str>,
}

impl Drop for InnerPostgresConnection {
    fn drop(&mut self) {
        io_error::cond.trap(|_| {}).inside(|| {
            self.write_messages([Terminate]);
        })
    }
}

impl InnerPostgresConnection {
    fn try_connect(url: &str, ssl: &SslMode)
            -> Result<InnerPostgresConnection, PostgresConnectError> {
        let Url {
            host,
            port,
            user,
            mut path,
            query: mut args,
            ..
        }: Url = match FromStr::from_str(url) {
            Some(url) => url,
            None => return Err(InvalidUrl)
        };

        let user = match user {
            Some(user) => user,
            None => return Err(MissingUser)
        };

        let port = match port {
            Some(port) => FromStr::from_str(port).unwrap(),
            None => DEFAULT_PORT
        };

        let stream = match initialize_stream(host, port, ssl) {
            Ok(stream) => stream,
            Err(err) => return Err(err)
        };

        let mut conn = InnerPostgresConnection {
            stream: BufferedStream::new(stream),
            next_stmt_id: 0,
            notice_handler: ~DefaultNoticeHandler as ~PostgresNoticeHandler,
            notifications: RingBuf::new(),
            cancel_data: PostgresCancelData { process_id: 0, secret_key: 0 },
            unknown_types: HashMap::new(),
        };

        args.push((~"client_encoding", ~"UTF8"));
        // Postgres uses the value of TimeZone as the time zone for TIMESTAMP
        // WITH TIME ZONE values. Timespec converts to GMT internally.
        args.push((~"TimeZone", ~"GMT"));
        // We have to clone here since we need the user again for auth
        args.push((~"user", user.user.clone()));
        if !path.is_empty() {
            // path contains the leading /
            path.shift_char();
            args.push((~"database", path));
        }
        conn.write_messages([StartupMessage {
            version: message::PROTOCOL_VERSION,
            parameters: args.as_slice()
        }]);

        match conn.handle_auth(user) {
            Some(err) => return Err(err),
            None => {}
        }

        loop {
            match conn.read_message() {
                BackendKeyData { process_id, secret_key } => {
                    conn.cancel_data.process_id = process_id;
                    conn.cancel_data.secret_key = secret_key;
                }
                ReadyForQuery { .. } => break,
                ErrorResponse { fields } =>
                    return Err(DbError(PostgresDbError::new(fields))),
                _ => unreachable!()
            }
        }

        Ok(conn)
    }

    fn write_messages(&mut self, messages: &[FrontendMessage]) {
        for message in messages.iter() {
            self.stream.write_message(message);
        }
        self.stream.flush();
    }

    fn read_message(&mut self) -> BackendMessage {
        loop {
            match self.stream.read_message() {
                NoticeResponse { fields } =>
                    self.notice_handler.handle(PostgresDbError::new(fields)),
                NotificationResponse { pid, channel, payload } =>
                    self.notifications.push_back(PostgresNotification {
                        pid: pid,
                        channel: channel,
                        payload: payload
                    }),
                ParameterStatus { parameter, value } =>
                    info!("Parameter {} = {}", parameter, value),
                msg => return msg
            }
        }
    }

    fn handle_auth(&mut self, user: UserInfo) -> Option<PostgresConnectError> {
        match self.read_message() {
            AuthenticationOk => return None,
            AuthenticationCleartextPassword => {
                let pass = match user.pass {
                    Some(pass) => pass,
                    None => return Some(MissingPassword)
                };
                self.write_messages([PasswordMessage { password: pass }]);
            }
            AuthenticationMD5Password { salt } => {
                let UserInfo { user, pass } = user;
                let pass = match pass {
                    Some(pass) => pass,
                    None => return Some(MissingPassword)
                };
                let input = pass + user;
                let hasher = Hasher::new(MD5);
                hasher.update(input.as_bytes());
                let output = hasher.final().to_hex();
                let hasher = Hasher::new(MD5);
                hasher.update(output.as_bytes());
                hasher.update(salt);
                let output = "md5" + hasher.final().to_hex();
                self.write_messages([PasswordMessage {
                    password: output.as_slice()
                }]);
            }
            AuthenticationKerberosV5
            | AuthenticationSCMCredential
            | AuthenticationGSS
            | AuthenticationSSPI => return Some(UnsupportedAuthentication),
            ErrorResponse { fields } =>
                return Some(DbError(PostgresDbError::new(fields))),
            _ => unreachable!()
        }

        match self.read_message() {
            AuthenticationOk => None,
            ErrorResponse { fields } =>
                Some(DbError(PostgresDbError::new(fields))),
            _ => unreachable!()
        }
    }

    fn set_notice_handler(&mut self, handler: ~PostgresNoticeHandler)
            -> ~PostgresNoticeHandler {
        ::std::util::replace(&mut self.notice_handler, handler)
    }

    fn try_prepare<'a>(&mut self, query: &str, conn: &'a PostgresConnection)
            -> Result<NormalPostgresStatement<'a>, PostgresDbError> {
        let stmt_name = format!("statement_{}", self.next_stmt_id);
        self.next_stmt_id += 1;

        let types = [];
        self.write_messages([
            Parse {
                name: stmt_name,
                query: query,
                param_types: types
            },
            Describe {
                variant: 'S' as u8,
                name: stmt_name
            },
            Sync]);

        match self.read_message() {
            ParseComplete => {}
            ErrorResponse { fields } => {
                self.wait_for_ready();
                return Err(PostgresDbError::new(fields));
            }
            _ => unreachable!()
        }

        let mut param_types: ~[PostgresType] = match self.read_message() {
            ParameterDescription { types } =>
                types.iter().map(|ty| PostgresType::from_oid(*ty)).collect(),
            _ => unreachable!()
        };

        let mut result_desc: ~[ResultDescription] = match self.read_message() {
            RowDescription { descriptions } =>
                descriptions.move_iter().map(|desc| {
                        ResultDescription::from_row_description_entry(desc)
                    }).collect(),
            NoData => ~[],
            _ => unreachable!()
        };

        self.wait_for_ready();

        // now that the connection is ready again, get unknown type names
        for param in param_types.mut_iter() {
            match *param {
                PgUnknownType { oid, .. } =>
                    *param = PgUnknownType {
                        name: self.get_type_name(oid),
                        oid: oid
                    },
                _ => {}
            }
        }

        for desc in result_desc.mut_iter() {
            match desc.ty {
                PgUnknownType { oid, .. } =>
                    desc.ty = PgUnknownType {
                        name: self.get_type_name(oid),
                        oid: oid
                    },
                _ => {}
            }
        }

        Ok(NormalPostgresStatement {
            conn: conn,
            name: stmt_name,
            param_types: param_types,
            result_desc: result_desc,
            next_portal_id: RefCell::new(0)
        })
    }

    fn get_type_name(&mut self, oid: Oid) -> ~str {
        match self.unknown_types.find(&oid) {
            Some(name) => return name.clone(),
            None => {}
        }
        let name = self.quick_query(
                format!("SELECT typname FROM pg_type WHERE oid={}", oid))[0][0]
                .unwrap();
        self.unknown_types.insert(oid, name.clone());
        name
    }

    fn wait_for_ready(&mut self) {
        match self.read_message() {
            ReadyForQuery { .. } => {}
            _ => unreachable!()
        }
    }

    fn quick_query(&mut self, query: &str) -> ~[~[Option<~str>]] {
        self.write_messages([Query { query: query }]);

        let mut result = ~[];
        loop {
            match self.read_message() {
                ReadyForQuery { .. } => break,
                DataRow { row } =>
                    result.push(row.move_iter().map(|opt|
                            opt.map(|b| str::from_utf8_owned(b))).collect()),
                ErrorResponse { fields } =>
                    fail!("Error: {}",
                           PostgresDbError::new(fields).to_str()),
                _ => {}
            }
        }
        result
    }
}

/// A connection to a Postgres database.
pub struct PostgresConnection {
    priv conn: RefCell<InnerPostgresConnection>
}

impl PostgresConnection {
    /// Attempts to create a new connection to a Postgres database.
    ///
    /// The URL should be provided in the normal format:
    ///
    /// ```
    /// postgres://user[:password]@host[:port][/database][?param1=val1[[&param2=val2]...]]
    /// ```
    ///
    /// The password may be omitted if not required. The default Postgres port
    /// (5432) is used if none is specified. The database name defaults to the
    /// username if not specified.
    pub fn try_connect(url: &str, ssl: &SslMode)
            -> Result<PostgresConnection, PostgresConnectError> {
        InnerPostgresConnection::try_connect(url, ssl).map(|conn| {
            PostgresConnection {
                conn: RefCell::new(conn)
            }
        })
    }

    /// A convenience wrapper around `try_connect`.
    ///
    /// # Failure
    ///
    /// Fails if there was an error connecting to the database.
    pub fn connect(url: &str, ssl: &SslMode) -> PostgresConnection {
        match PostgresConnection::try_connect(url, ssl) {
            Ok(conn) => conn,
            Err(err) => fail!("Failed to connect: {}", err.to_str())
        }
    }

    /// Sets the notice handler for the connection, returning the old handler.
    pub fn set_notice_handler(&self, handler: ~PostgresNoticeHandler)
            -> ~PostgresNoticeHandler {
        let mut conn = self.conn.borrow_mut();
        conn.get().set_notice_handler(handler)
    }

    /// Returns an iterator over asynchronous notification messages.
    ///
    /// Use the `LISTEN` command to register this connection for notifications.
    pub fn notifications<'a>(&'a self) -> PostgresNotificationIterator<'a> {
        PostgresNotificationIterator {
            conn: self
        }
    }

    /// Attempts to create a new prepared statement.
    ///
    /// A statement may contain parameters, specified by `$n` where `n` is the
    /// index of the parameter in the list provided at execution time,
    /// 1-indexed.
    ///
    /// The statement is associated with the connection that created it and may
    /// not outlive that connection.
    pub fn try_prepare<'a>(&'a self, query: &str)
            -> Result<NormalPostgresStatement<'a>, PostgresDbError> {
        self.conn.with_mut(|conn| conn.try_prepare(query, self))
    }

    /// A convenience wrapper around `try_prepare`.
    ///
    /// # Failure
    ///
    /// Fails if there was an error preparing the statement.
    pub fn prepare<'a>(&'a self, query: &str) -> NormalPostgresStatement<'a> {
        match self.try_prepare(query) {
            Ok(stmt) => stmt,
            Err(err) => fail!("Error preparing statement:\n{}",
                               err.pretty_error(query))
        }
    }

    /// Begins a new transaction.
    ///
    /// Returns a `PostgresTransaction` object which should be used instead of
    /// the connection for the duration of the transaction. The transaction
    /// is active until the `PostgresTransaction` object falls out of scope.
    /// A transaction will commit by default unless the task fails or the
    /// transaction is set to roll back.
    pub fn transaction<'a>(&'a self) -> PostgresTransaction<'a> {
        self.quick_query("BEGIN");
        PostgresTransaction {
            conn: self,
            commit: RefCell::new(true),
            nested: false
        }
    }

    /// A convenience function for queries that are only run once.
    ///
    /// If an error is returned, it could have come from either the preparation
    /// or execution of the statement.
    ///
    /// On success, returns the number of rows modified or 0 if not applicable.
    pub fn try_execute(&self, query: &str, params: &[&ToSql])
            -> Result<uint, PostgresDbError> {
        self.try_prepare(query).and_then(|stmt| stmt.try_execute(params))
    }

    /// A convenience wrapper around `try_execute`.
    ///
    /// # Failure
    ///
    /// Fails if there was an error preparing or executing the statement.
    pub fn execute(&self, query: &str, params: &[&ToSql]) -> uint {
        match self.try_execute(query, params) {
            Ok(res) => res,
            Err(err) => fail!("Error running query:\n{}",
                               err.pretty_error(query))
        }
    }

    /// Returns information used to cancel pending queries.
    ///
    /// Used with the `cancel_query` function. The object returned can be used
    /// to cancel any query executed by the connection it was created from.
    pub fn cancel_data(&self) -> PostgresCancelData {
        self.conn.with(|conn| conn.cancel_data)
    }

    fn quick_query(&self, query: &str) -> ~[~[Option<~str>]] {
        self.conn.with_mut(|conn| conn.quick_query(query))
    }

    fn wait_for_ready(&self) {
        self.conn.with_mut(|conn| conn.wait_for_ready())
    }

    fn read_message(&self) -> BackendMessage {
        self.conn.with_mut(|conn| conn.read_message())
    }

    fn write_messages(&self, messages: &[FrontendMessage]) {
        self.conn.with_mut(|conn| conn.write_messages(messages))
    }
}

/// Specifies the SSL support requested for a new connection
pub enum SslMode {
    /// The connection will not use SSL
    NoSsl,
    /// The connection will use SSL if the backend supports it
    PreferSsl(SslContext),
    /// The connection must use SSL
    RequireSsl(SslContext)
}

/// Represents a transaction on a database connection
pub struct PostgresTransaction<'conn> {
    priv conn: &'conn PostgresConnection,
    priv commit: RefCell<bool>,
    priv nested: bool
}

#[unsafe_destructor]
impl<'conn> Drop for PostgresTransaction<'conn> {
    fn drop(&mut self) {
        io_error::cond.trap(|_| {}).inside(|| {
            if task::failing() || !self.commit.with(|x| *x) {
                if self.nested {
                    self.conn.quick_query("ROLLBACK TO sp");
                } else {
                    self.conn.quick_query("ROLLBACK");
                }
            } else {
                if self.nested {
                    self.conn.quick_query("RELEASE sp");
                } else {
                    self.conn.quick_query("COMMIT");
                }
            }
        })
    }
}

impl<'conn> PostgresTransaction<'conn> {
    /// Like `PostgresConnection::try_prepare`.
    pub fn try_prepare<'a>(&'a self, query: &str)
            -> Result<TransactionalPostgresStatement<'a>, PostgresDbError> {
        self.conn.try_prepare(query).map(|stmt| {
            TransactionalPostgresStatement {
                stmt: stmt
            }
        })
    }

    /// Like `PostgresConnection::prepare`.
    pub fn prepare<'a>(&'a self, query: &str)
            -> TransactionalPostgresStatement<'a> {
        TransactionalPostgresStatement {
            stmt: self.conn.prepare(query)
        }
    }

    /// Like `PostgresConnection::try_execute`.
    pub fn try_execute(&self, query: &str, params: &[&ToSql])
            -> Result<uint, PostgresDbError> {
        self.conn.try_execute(query, params)
    }

    /// Like `PostgresConnection::execute`.
    pub fn execute(&self, query: &str, params: &[&ToSql]) -> uint {
        self.conn.execute(query, params)
    }

    /// Like `PostgresConnection::transaction`.
    pub fn transaction<'a>(&self) -> PostgresTransaction<'conn> {
        self.conn.quick_query("SAVEPOINT sp");
        PostgresTransaction {
            conn: self.conn,
            commit: RefCell::new(true),
            nested: true
        }
    }

    /// Like `PostgresConnection::notifications`.
    pub fn notifications<'a>(&'a self) -> PostgresNotificationIterator<'a> {
        self.conn.notifications()
    }

    /// Determines if the transaction is currently set to commit or roll back.
    pub fn will_commit(&self) -> bool {
        self.commit.with(|x| *x)
    }

    /// Sets the transaction to commit at its completion.
    pub fn set_commit(&self) {
        self.commit.with_mut(|x| *x = true);
    }

    /// Sets the transaction to roll back at its completion.
    pub fn set_rollback(&self) {
        self.commit.with_mut(|x| *x = false);
    }
}

/// A trait containing methods that can be called on a prepared statement.
pub trait PostgresStatement {
    /// Returns a slice containing the expected parameter types.
    fn param_types<'a>(&'a self) -> &'a [PostgresType];

    /// Returns a slice describing the columns of the result of the query.
    fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription];

    /// Attempts to execute the prepared statement, returning the number of
    /// rows modified.
    ///
    /// If the statement does not modify any rows (e.g. SELECT), 0 is returned.
    ///
    /// # Failure
    ///
    /// Fails if the number or types of the provided parameters do not match
    /// the parameters of the statement.
    fn try_execute(&self, params: &[&ToSql]) -> Result<uint, PostgresDbError>;

    /// A convenience function wrapping `try_execute`.
    ///
    /// # Failure
    ///
    /// Fails if there was an error executing the statement.
    fn execute(&self, params: &[&ToSql]) -> uint {
        match self.try_execute(params) {
            Ok(count) => count,
            Err(err) => fail!("Error running query\n{}", err.to_str())
        }
    }

    /// Attempts to execute the prepared statement, returning an iterator over
    /// the resulting rows.
    ///
    /// # Failure
    ///
    /// Fails if the number or types of the provided parameters do not match
    /// the parameters of the statement.
    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError>;

    /// A convenience function wrapping `try_query`.
    ///
    /// # Failure
    ///
    /// Fails if there was an error executing the statement.
    fn query<'a>(&'a self, params: &[&ToSql]) -> PostgresResult<'a> {
        match self.try_query(params) {
            Ok(result) => result,
            Err(err) => fail!("Error executing query:\n{}", err.to_str())
        }
    }
}

/// A statement prepared outside of a transaction.
pub struct NormalPostgresStatement<'conn> {
    priv conn: &'conn PostgresConnection,
    priv name: ~str,
    priv param_types: ~[PostgresType],
    priv result_desc: ~[ResultDescription],
    priv next_portal_id: RefCell<uint>
}

#[unsafe_destructor]
impl<'conn> Drop for NormalPostgresStatement<'conn> {
    fn drop(&mut self) {
        io_error::cond.trap(|_| {}).inside(|| {
            self.conn.write_messages([
                Close {
                    variant: 'S' as u8,
                    name: self.name.as_slice()
                },
                Sync]);
            loop {
                match self.conn.read_message() {
                    ReadyForQuery { .. } => break,
                    _ => {}
                }
            }
        })
    }
}

impl<'conn> NormalPostgresStatement<'conn> {
    fn execute(&self, portal_name: &str, row_limit: uint, params: &[&ToSql])
            -> Option<PostgresDbError> {
        let mut formats = ~[];
        let mut values = ~[];
        assert!(self.param_types.len() == params.len(),
                "Expected {} parameters but found {}",
                self.param_types.len(), params.len());
        for (&param, ty) in params.iter().zip(self.param_types.iter()) {
            let (format, value) = param.to_sql(ty);
            formats.push(format as i16);
            values.push(value);
        };

        let result_formats: ~[i16] = self.result_desc.iter().map(|desc| {
            desc.ty.result_format() as i16
        }).collect();

        self.conn.write_messages([
            Bind {
                portal: portal_name,
                statement: self.name.as_slice(),
                formats: formats,
                values: values,
                result_formats: result_formats
            },
            Execute {
                portal: portal_name,
                max_rows: row_limit as i32
            },
            Sync]);

        match self.conn.read_message() {
            BindComplete => None,
            ErrorResponse { fields } => {
                self.conn.wait_for_ready();
                Some(PostgresDbError::new(fields))
            }
            _ => unreachable!()
        }
    }

    fn try_lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        let id = self.next_portal_id.with_mut(|x| { *x += 1; *x - 1 });
        let portal_name = format!("{}_portal_{}", self.name, id);

        match self.execute(portal_name, row_limit, params) {
            Some(err) => return Err(err),
            None => {}
        }

        let mut result = PostgresResult {
            stmt: self,
            name: portal_name,
            data: RingBuf::new(),
            row_limit: row_limit,
            more_rows: true
        };
        result.read_rows();

        Ok(result)
    }
}

impl<'conn> PostgresStatement for NormalPostgresStatement<'conn> {
    fn param_types<'a>(&'a self) -> &'a [PostgresType] {
        self.param_types.as_slice()
    }

    fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription] {
        self.result_desc.as_slice()
    }

    fn try_execute(&self, params: &[&ToSql])
                      -> Result<uint, PostgresDbError> {
        match self.execute("", 0, params) {
            Some(err) => return Err(err),
            None => {}
        }

        let num;
        loop {
            match self.conn.read_message() {
                DataRow { .. } => {}
                ErrorResponse { fields } => {
                    self.conn.wait_for_ready();
                    return Err(PostgresDbError::new(fields));
                }
                CommandComplete { tag } => {
                    let s = tag.split(' ').last().unwrap();
                    num = match FromStr::from_str(s) {
                        None => 0,
                        Some(n) => n
                    };
                    break;
                }
                EmptyQueryResponse => {
                    num = 0;
                    break;
                }
                _ => unreachable!()
            }
        }
        self.conn.wait_for_ready();

        Ok(num)
    }

    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.try_lazy_query(0, params)
    }
}

/// Information about a column of the result of a query.
#[deriving(Eq)]
pub struct ResultDescription {
    /// The name of the column
    name: ~str,
    /// The type of the data in the column
    ty: PostgresType
}

impl ResultDescription {
    fn from_row_description_entry(row: RowDescriptionEntry)
            -> ResultDescription {
        let RowDescriptionEntry { name, type_oid, .. } = row;

        ResultDescription {
            name: name,
            ty: PostgresType::from_oid(type_oid)
        }
    }
}

/// A statement prepared inside of a transaction.
///
/// Provides additional functionality over a `NormalPostgresStatement`.
pub struct TransactionalPostgresStatement<'conn> {
    priv stmt: NormalPostgresStatement<'conn>
}

impl<'conn> PostgresStatement for TransactionalPostgresStatement<'conn> {
    fn param_types<'a>(&'a self) -> &'a [PostgresType] {
        self.stmt.param_types()
    }

    fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription] {
        self.stmt.result_descriptions()
    }

    fn try_execute(&self, params: &[&ToSql]) -> Result<uint, PostgresDbError> {
        self.stmt.try_execute(params)
    }

    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.stmt.try_query(params)
    }
}

impl<'conn> TransactionalPostgresStatement<'conn> {
    /// Attempts to execute the prepared statement, returning a lazily loaded
    /// iterator over the resulting rows.
    ///
    /// No more than `row_limit` rows will be stored in memory at a time. Rows
    /// will be pulled from the database in batches of `row_limit` as needed.
    /// If `row_limit` is 0, `try_lazy_query` is equivalent to `try_query`.
    ///
    /// # Failure
    ///
    /// Fails if the number or types of the provided parameters do not match
    /// the parameters of the statement.
    pub fn try_lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.stmt.try_lazy_query(row_limit, params)
    }

    /// A convenience wrapper around `try_lazy_query`.
    ///
    /// # Failure
    ///
    /// Fails if there was an error executing the statement.
    pub fn lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<'a> {
        match self.try_lazy_query(row_limit, params) {
            Ok(result) => result,
            Err(err) => fail!("Error executing query:\n{}", err.to_str())
        }
    }
}

/// An iterator over the resulting rows of a query.
pub struct PostgresResult<'stmt> {
    priv stmt: &'stmt NormalPostgresStatement<'stmt>,
    priv name: ~str,
    priv data: RingBuf<~[Option<~[u8]>]>,
    priv row_limit: uint,
    priv more_rows: bool
}

#[unsafe_destructor]
impl<'stmt> Drop for PostgresResult<'stmt> {
    fn drop(&mut self) {
        io_error::cond.trap(|_| {}).inside(|| {
            self.stmt.conn.write_messages([
                Close {
                    variant: 'P' as u8,
                    name: self.name.as_slice()
                },
                Sync]);
            loop {
                match self.stmt.conn.read_message() {
                    ReadyForQuery { .. } => break,
                    _ => {}
                }
            }
        })
    }
}

impl<'stmt> PostgresResult<'stmt> {
    fn read_rows(&mut self) {
        loop {
            match self.stmt.conn.read_message() {
                EmptyQueryResponse |
                CommandComplete { .. } => {
                    self.more_rows = false;
                    break;
                },
                PortalSuspended => {
                    self.more_rows = true;
                    break;
                },
                DataRow { row } => self.data.push_back(row),
                _ => unreachable!()
            }
        }
        self.stmt.conn.wait_for_ready();
    }

    fn execute(&mut self) {
        self.stmt.conn.write_messages([
            Execute {
                portal: self.name,
                max_rows: self.row_limit as i32
            },
            Sync]);
        self.read_rows();
    }
}

impl<'stmt> Iterator<PostgresRow<'stmt>> for PostgresResult<'stmt> {
    fn next(&mut self) -> Option<PostgresRow<'stmt>> {
        if self.data.is_empty() && self.more_rows {
            self.execute();
        }

        self.data.pop_front().map(|row| {
            PostgresRow {
                stmt: self.stmt,
                data: row
            }
        })
    }
}

/// A single result row of a query.
///
/// A value can be accessed by the name or index of its column, though access
/// by index is more efficient. Rows are 1-indexed.
///
/// ```rust
/// let foo: i32 = row[1];
/// let bar: ~str = row["bar"];
/// ```
pub struct PostgresRow<'stmt> {
    priv stmt: &'stmt NormalPostgresStatement<'stmt>,
    priv data: ~[Option<~[u8]>]
}

impl<'stmt> Container for PostgresRow<'stmt> {
    #[inline]
    fn len(&self) -> uint {
        self.data.len()
    }
}

impl<'stmt, I: RowIndex, T: FromSql> Index<I, T> for PostgresRow<'stmt> {
    #[inline]
    fn index(&self, idx: &I) -> T {
        let idx = idx.idx(self.stmt);
        FromSql::from_sql(&self.stmt.result_desc[idx].ty, &self.data[idx])
    }
}

/// A trait implemented by types that can index into columns of a row.
pub trait RowIndex {
    /// Returns the index of the appropriate column.
    ///
    /// # Failure
    ///
    /// Fails if there is no corresponding column.
    fn idx(&self, stmt: &NormalPostgresStatement) -> uint;
}

impl RowIndex for uint {
    #[inline]
    fn idx(&self, _stmt: &NormalPostgresStatement) -> uint {
        assert!(*self != 0, "out of bounds row access");
        *self - 1
    }
}

// This is a convenience as the 1 in get[1] resolves to int :(
impl RowIndex for int {
    #[inline]
    fn idx(&self, _stmt: &NormalPostgresStatement) -> uint {
        assert!(*self >= 1, "out of bounds row access");
        (*self - 1) as uint
    }
}

impl<'a> RowIndex for &'a str {
    fn idx(&self, stmt: &NormalPostgresStatement) -> uint {
        for (i, desc) in stmt.result_descriptions().iter().enumerate() {
            if desc.name.as_slice() == *self {
                return i;
            }
        }
        fail!("There is no column with name {}", *self);
    }
}
