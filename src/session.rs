//! A session which allows HTTP applications to associate data with visitors.
use std::{
    error::Error,
    fmt::{self, Debug, Display},
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use axum_core::{
    body::Body,
    extract::FromRequestParts,
    response::{IntoResponse, Response},
};

use http::request::Parts;

// TODO: Remove send + sync bounds on `R` once return type notation is stable.

use tower_sessions_core::{expires::Expires, id::Id, Expiry, SessionStore};

#[derive(Debug, Clone, Copy)]
pub(crate) enum SessionUpdate {
    Delete,
    Set(Id, Expiry),
}

pub(crate) type Updater = Arc<Mutex<Option<SessionUpdate>>>;

/// A session that is lazily loaded.
///
/// This is struct provided throught the Request's Extensions by the [`SessionManager`] middleware.
/// If you happen to use `axum`, you can use this struct as an extractor since it implements
/// [`FromRequestParts`].
///
/// This struct has a somewhat convoluted API, but it is designed to be nearly impossible to
/// misuse. Luckily, it only has a handful of methods, and each of them document how they work.
///
/// When this struct refers to the "underlying store error", it is referring to the fact that the
/// store used returned a "hard" error. For example, it could be a connection error, a protocol error,
/// a timeout, etc. A counterexample would be the [`SessionState`] not being found in the store, which is
/// not considered an error by the [`SessionStore`] trait.
pub struct Session<Store> {
    /// This will be `None` if the handler has not received a session cookie or if the it could
    /// not be parsed.
    pub(crate) id: Option<Id>,
    pub(crate) store: Store,
    pub(crate) updater: Updater,
}

impl<Store> Clone for Session<Store>
where
    Store: Clone,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            store: self.store.clone(),
            updater: self.updater.clone(),
        }
    }
}

impl<Store: Debug> Debug for Session<Store> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("store", &self.store)
            .field("id", &self.id)
            .finish()
    }
}

impl<Store> Session<Store> {
    /// Try to load the session from the store.
    ///
    /// The return type of this method looks convoluted, so let's break it down:
    /// - The outer `Result` will return `Err(...)` if the underlying session store errors.
    /// - Otherwise, it will return `Ok(...)`, where `...` is an `Option`.
    /// - The inner `Option` will be `None` if the session was not found in the store.
    /// - Otherwise, it will be `Some(...)`, where `...` is the loaded session.
    pub async fn load<R>(mut self) -> Result<Option<SessionState<R, Store>>, Store::Error>
    where
        R: Send + Sync,
        Store: SessionStore<R>,
    {
        Ok(if let Some(id) = self.id {
            self.store.load(&id).await?.map(|data| SessionState {
                store: self.store,
                id,
                data,
                updater: self.updater,
            })
        } else {
            None
        })
    }

    /// Create a new session with the given data.
    ///
    /// # Error
    ///
    /// Errors if the underlying store errors.
    pub async fn create<R>(mut self, data: R) -> Result<SessionState<R, Store>, Store::Error>
        where
            R: Expires + Send + Sync,
            Store: SessionStore<R> {
        let id = self.store.create(&data).await?;
        self.updater
            .lock()
            .expect("lock should not be poisoned")
            .replace(SessionUpdate::Set(id, data.expires()));
        Ok(SessionState {
            store: self.store,
            id,
            data,
            updater: self.updater,
        })
    }
}

#[derive(Debug, Clone, Copy)]
/// A rejection that is returned from the [`Session`] extractor when the [`SessionManagerLayer`]
/// middleware is not set.
pub struct NoMiddleware;

impl Display for NoMiddleware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Missing session middleware. Is it added to the app?")
    }
}

impl Error for NoMiddleware {}

impl IntoResponse for NoMiddleware {
    fn into_response(self) -> Response {
        let mut resp = Response::new(Body::from(self.to_string()));
        *resp.status_mut() = http::StatusCode::INTERNAL_SERVER_ERROR;
        resp
    }
}

#[async_trait::async_trait]
impl<State, Store> FromRequestParts<State> for Session<Store>
where
    Store: Send + Sync + 'static,
{
    type Rejection = NoMiddleware;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &State,
    ) -> Result<Self, Self::Rejection> {
        let session = parts
            .extensions
            .remove::<Session<Store>>()
            .ok_or(NoMiddleware)?;

        Ok(session)
    }
}

/// A loaded session.
///
/// This struct has a somewhat convoluted API, but it is designed to be nearly impossible to
/// misuse. Luckily, it only has a handful of methods, and each of them document how they work.
///
/// When this struct refers to the "underlying store error", it is referring to the fact that the
/// store used returned a "hard" error. For example, it could be a connection error, a protocol error,
/// a timeout, etc. A counterexample would be the session not being found in the store, which is
/// not considered an error by the `SessionStore` trait.
#[derive(Debug, Clone)]
pub struct SessionState<R, Store> {
    store: Store,
    id: Id,
    data: R,
    updater: Updater,
}

impl<R, Store> SessionState<R, Store> {
    /// Read the data associated with the session.
    pub fn data(&self) -> &R {
        &self.data
    }
    /// Mutably access the data associated with the session.
    ///
    /// Returns a [`DataMut`], which functions similarly to a `Guard`.
    pub fn data_mut(self) -> DataMut<R, Store> {
        DataMut { session: self }
    }
}

impl<R, Store> SessionState<R, Store>
where
    R: Expires + Send + Sync,
    Store: SessionStore<R>,
{
    /// Delete the session from the store.
    ///
    /// This method returns a boolean indicating whether the session was deleted from the store.
    /// If the `Store` returns `Ok(false)` if the session simply did not exist. This can happen if
    /// it was deleted by another request or if the session expired between the time it was
    /// loaded and the time this method was called.
    ///
    /// # Error
    ///
    /// Errors if the underlying store errors.
    pub async fn delete(mut self) -> Result<bool, Store::Error> {
        let deleted = self.store.delete(&self.id).await?;
        self.updater
            .lock()
            .expect("lock should not be poisoned")
            .replace(SessionUpdate::Delete);
        let _ = ManuallyDrop::new(self);
        Ok(deleted)
    }

    /// Cycle the session ID.
    ///
    /// This consumes the current session and returns a new session with the new ID. This method
    /// should be used to mitigate [session fixation attacks](https://www.acrossecurity.com/papers/session_fixation.pdf).
    ///
    /// This method returns `Ok(None)` if the session was deleted or expired between the time it
    /// was loaded and the time this method was called. Otherwise, it returns the new
    /// `Some(Session)`.
    ///
    /// # Error
    ///
    /// Errors if the underlying store errors.
    pub async fn cycle(mut self) -> Result<Option<SessionState<R, Store>>, Store::Error> {
        if let Some(new_id) = self.store.cycle_id(&self.id).await? {
            self.updater
                .lock()
                .expect("lock should not be poisoned")
                .replace(SessionUpdate::Set(new_id, self.data.expires()));
            self.id = new_id;
            return Ok(Some(self));
        }
        Ok(None)
    }
}

/// A struct that provides mutable access to a session's data.
/// Access to `R` is provided through `Deref` and `DerefMut`.
///
/// This is created by calling `data_mut` on a `Session`.
/// To retrieve the `Session`, call `save` on this struct.
///
/// You should save the session data by calling `save` before dropping this struct.
#[derive(Debug)]
#[must_use = "You should call `save` before dropping this struct"]
pub struct DataMut<R, Store> {
    session: SessionState<R, Store>,
}

impl<R: Send + Sync, Store: SessionStore<R>> DataMut<R, Store> {
    /// Save the session data to the store.
    ///
    /// This method returns the `Session` if the data was saved successfully. It returns
    /// `Ok(None)` when the session was deleted or expired between the time it was loaded and the
    /// time this method is called.
    ///
    /// # Error
    ///
    /// Errors if the underlying store errors.
    pub async fn save(mut self) -> Result<Option<SessionState<R, Store>>, Store::Error> {
        Ok(self
            .session
            .store
            .save(&self.session.id, &self.session.data)
            .await?
            .then_some(self.session))
    }
}

impl<R, Store> Deref for DataMut<R, Store> {
    type Target = R;

    fn deref(&self) -> &Self::Target {
        &self.session.data
    }
}

impl<R, Store> DerefMut for DataMut<R, Store> {
    fn deref_mut(&mut self) -> &mut R {
        &mut self.session.data
    }
}
