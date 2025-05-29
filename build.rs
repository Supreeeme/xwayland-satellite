use vergen_gitcl::{Emitter, GitclBuilder};

fn main() -> Result<(), anyhow::Error> {
    let builder = GitclBuilder::default().describe(true, true, None).build()?;
    Emitter::default().add_instructions(&builder)?.emit()
}
