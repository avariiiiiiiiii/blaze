//! A dev build's way to its dependency image: [`plan`] splits the build, keys the image of
//! its dependency pieces and picks the image it links against from the machine's
//! [`Images`]; a [`Job`] makes an image (from the store when it is there, else by the
//! prelink) for [`Images::finish`].
//!
//! The daemon's dev run (`v2/requests/fastcompile.md`, fastcompile-premap 5):
//!
//! 1. [`plan`]. On [`Plan::Make`], run the job now (the graph has no image yet), finish it,
//!    plan again; on [`Plan::Wait`], wait for the build making it.
//! 2. On [`Plan::Link`], link the workspace part with the image's table as its outside
//!    symbols, and run with the image's maps and [`crate::start::plan`]'s start; run `next`,
//!    when set, at idle priority and finish it, so the next build links against it.

use std::sync::Arc;

use blaze_ids::ContentId;

use crate::PremapError;
use crate::cache::{Images, Pick, Ready};
use crate::key::{GraphInputs, GraphKey, ImageInputs, ImageKey};
use crate::prelink::{self, Prelink, Request};
use crate::split::{Built, ImagePart, Owned, SidedNative, WorkspacePart, image_part, workspace_part};
use crate::store::Shelf;

/// A dev build, as premap takes it.
pub struct Dev<'a> {
    pub graph: GraphInputs<'a>,
    pub built: Built<'a>,
    pub owned: Owned<'a>,
    pub natives: &'a [SidedNative<'a>],
    /// The span tables the graph's span keys name, as the linker fills positions from.
    pub spans: &'a [(ContentId, &'a [u8])],
}

pub enum Plan<'a> {
    /// Link `workspace` against `image`. `next` is set when the image lacks dependency
    /// pieces of this build: the image of its key, claimed, to make in the background.
    Link { image: Arc<Ready>, workspace: WorkspacePart<'a>, next: Option<Job<'a>> },
    /// The graph has no image: make this one, claimed, and plan again.
    Make(Job<'a>),
    /// Another build is making the graph's first image: wait for it and plan again.
    Wait,
}

/// An image to make, claimed in [`Images`].
pub struct Job<'a> {
    pub graph: GraphKey,
    pub key: ImageKey,
    part: ImagePart<'a>,
    dev: &'a Dev<'a>,
}

pub fn plan<'a>(images: &mut Images, dev: &'a Dev<'a>) -> Result<Plan<'a>, PremapError> {
    let part = image_part(&dev.built, &dev.owned, dev.natives)?;
    let graph = GraphKey::of(&dev.graph);
    let key = ImageKey::of(
        graph,
        &ImageInputs {
            functions: &part.keyed_functions,
            statics: &part.keyed_statics,
            names: &part.names,
            debug_names: &part.debug_names,
        },
    );
    match images.pick(graph, key) {
        Pick::Use(image) => {
            let workspace = workspace_part(&dev.built, &dev.owned, &image.view().table(), dev.natives)?;
            if workspace.complete() {
                return Ok(Plan::Link { image, workspace, next: None });
            }
            if image.key == key {
                return Err(PremapError::new(format!(
                    "the dependency image {:?} lacks {} functions and {} statics its own pieces have",
                    key.0,
                    workspace.spilled.len(),
                    workspace.spilled_statics.len()
                )));
            }
            let next = images.claim(key).then_some(Job { graph, key, part, dev });
            Ok(Plan::Link { image, workspace, next })
        }
        Pick::Make => Ok(Plan::Make(Job { graph, key, part, dev })),
        Pick::Wait => Ok(Plan::Wait),
        Pick::Failed(error) => Err(error),
    }
}

impl Job<'_> {
    /// The image, from `shelf` when it keeps it, else linked by `linker` and kept there.
    pub fn make(&self, linker: &impl Prelink, shelf: &mut impl Shelf) -> Result<Ready, PremapError> {
        if let Some(bytes) = shelf.get_image(&self.key)? {
            return Ready::open(self.graph, self.key, bytes);
        }
        let name = format!("blaze-premap-{}", self.key.0);
        let output = prelink::output(self.dev.graph.output, &name);
        let request = Request {
            output: &output,
            base: self.dev.graph.base,
            tls_bias: self.dev.graph.tls_bias,
            functions: &self.part.functions,
            statics: &self.part.statics,
            names: &self.part.names,
            debug_names: &self.part.debug_names,
            natives: &self.part.natives,
            spans: self.dev.spans,
        };
        let bytes = prelink::build(linker, &request, self.key)?;
        shelf.put_image(&self.key, bytes.clone())?;
        Ready::open(self.graph, self.key, bytes.into_boxed_slice())
    }
}
