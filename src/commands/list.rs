use tendr::model::ids::Namespace;
use tendr::session::{self, SessionRoot};

pub fn cmd_list(namespace: Option<&Namespace>) -> anyhow::Result<()> {
    let root = SessionRoot::default_path()?;
    let sessions = session::list(&root, namespace)?;

    let entries: Vec<serde_json::Value> = sessions
        .iter()
        .map(|(ns, name)| {
            serde_json::json!({
                "namespace": ns.as_str(),
                "name": name.as_str(),
            })
        })
        .collect();
    let json = serde_json::to_string_pretty(&entries)?;
    outln!("{json}");

    Ok(())
}
