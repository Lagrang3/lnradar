use cln_plugin::{ConfiguredPlugin, Plugin};
use cln_rpc::ClnRpc;
use std::path::Path;

pub trait FromPlugin<P> {
    async fn from_plugin(plugin: &P) -> anyhow::Result<Self>
    where
        Self: Sized;
}

impl<S: Clone + Send> FromPlugin<Plugin<S>> for ClnRpc {
    async fn from_plugin(plugin: &Plugin<S>) -> anyhow::Result<Self> {
        ClnRpc::new(
            Path::new(&plugin.configuration().lightning_dir).join(plugin.configuration().rpc_file),
        )
        .await
    }
}

impl<
        S: Clone + Send + Sync + 'static,
        I: tokio::io::AsyncRead + Send + Unpin + 'static,
        O: Send + tokio::io::AsyncWrite + Unpin + 'static,
    > FromPlugin<ConfiguredPlugin<S, I, O>> for ClnRpc
{
    async fn from_plugin(plugin: &ConfiguredPlugin<S, I, O>) -> anyhow::Result<Self> {
        ClnRpc::new(
            Path::new(&plugin.configuration().lightning_dir).join(plugin.configuration().rpc_file),
        )
        .await
    }
}
