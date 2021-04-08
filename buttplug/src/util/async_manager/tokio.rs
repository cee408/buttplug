use tokio;
use futures::{
  future::{Future, RemoteHandle},
  task::{FutureObj, Spawn, SpawnError, SpawnExt},
};

#[derive(Default)]
pub struct TokioAsyncManager {}

impl Spawn for TokioAsyncManager {
  fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
    tokio::spawn(future);
    Ok(())
  }
}

pub fn spawn<Fut>(future: Fut) -> Result<(), SpawnError>
where
  Fut: Future<Output = ()> + Send + 'static,
{
  TokioAsyncManager::default().spawn(future)
}

pub fn spawn_with_handle<Fut>(future: Fut) -> Result<RemoteHandle<Fut::Output>, SpawnError>
where
  Fut: Future + Send + 'static,
  Fut::Output: Send,
{
  TokioAsyncManager::default().spawn_with_handle(future)
}

pub fn block_on<F>(f: F) -> <F as Future>::Output
where
  F: Future,
{
  // Create the runtime
  let rt  = tokio::runtime::Runtime::new().unwrap();

  // Execute the future, blocking the current thread until completion
  rt.block_on(async move {
    f.await
  })
}