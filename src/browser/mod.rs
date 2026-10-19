use crate::platform::{Platform, WindowHandle};

pub mod firefox;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserWindow<M> {
    pub handle: WindowHandle,
    pub metadata: M,
}

/// Browser-specific launch, discovery, and readiness behavior.
#[allow(async_fn_in_trait)]
pub trait BrowserAdapter: Send + Sync {
    type Metadata: Clone + Send + Sync;

    fn identifies(&self, app_id: Option<&str>) -> bool;
    fn metadata(&self, app_id: Option<String>, title: String) -> Self::Metadata;
    fn launch_new_window(&self) -> Result<(), String>;

    async fn acquire_windows<P: Platform>(
        &self,
        platform: &P,
        count: usize,
        open_browser: bool,
    ) -> Result<Vec<BrowserWindow<Self::Metadata>>, String>;
}
