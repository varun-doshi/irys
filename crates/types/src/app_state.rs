use reth_db::DatabaseEnv;
use std::{ops::Deref, sync::Arc};

pub struct AppState {}
#[derive(Debug, Clone)]
pub struct DatabaseProvider(pub Arc<DatabaseEnv>);

impl Deref for DatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
