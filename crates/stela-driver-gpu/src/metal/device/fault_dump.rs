//! A lost device's fault dump ([`crate::metal::fault_dump`]): the pipelines
//! its unfinished command buffers bound, each stage as the driver compiled
//! it.
use super::Device;
use crate::metal::{
    command_buffer::BoundPipeline,
    fault_dump::{
        self, HEX_BUDGET, HexBudget, MAX_PIPELINES, MAX_VARIANTS, ShaderImage, more_variants,
        no_shaders, released,
    },
    shader::Compiler,
    vulkan::backend::Backend,
};
use alloc::{format, string::String, vec, vec::Vec};

/// How a dump names a pipeline: its kind and device id.
fn label(pipeline: BoundPipeline) -> String {
    match pipeline {
        BoundPipeline::Render(id) => format!("render {id}"),
        BoundPipeline::Compute(id) => format!("compute {id}"),
    }
}

impl<B: Backend, C: Compiler> Device<B, C> {
    /// The lines of the boot's fault `fault` (from 1), for the log once the
    /// device is lost: the command buffers it had not finished and the
    /// pipelines they bound, render pipelines first (a program header the
    /// GPU rejects is a graphics stage's), then a line for each stage of
    /// each with its program header, and for the boot's first fault the
    /// code in hex within [`HEX_BUDGET`].
    pub fn fault_dump(&mut self, fault: u32) -> Vec<String> {
        let (serials, mut pipelines) = self.unfinished_pipelines();
        // Stable: each kind keeps the order it was bound in.
        pipelines.sort_by_key(|pipeline| matches!(pipeline, BoundPipeline::Compute(_)));
        let labels: Vec<String> = pipelines.iter().copied().map(label).collect();
        let mut lines = vec![fault_dump::opening(fault, &serials, &labels)];
        // Where each stopped, before the pipelines' code: the span a hang
        // was in names the few pipelines worth reading of the many dumped.
        for (serial, progress) in self.unfinished_progress() {
            lines.push(fault_dump::progress(fault, serial, &progress));
        }
        let mut stages: Vec<(&str, usize, ShaderImage)> = Vec::new();
        for (&pipeline, label) in pipelines.iter().zip(&labels).take(MAX_PIPELINES) {
            let (Ok(functions), Ok(vulkan)) = (
                self.pipelines.entries(pipeline),
                self.pipelines.vulkan_pipelines(pipeline),
            ) else {
                lines.push(released(fault, label));
                continue;
            };
            let functions = functions.join("+");
            for (variant, &made) in vulkan.iter().enumerate().take(MAX_VARIANTS) {
                let images = self.backend.shader_images(made);
                if images.is_empty() {
                    lines.push(no_shaders(fault, label, variant, &functions));
                }
                for image in images {
                    lines.push(fault_dump::shader(
                        fault, label, variant, &functions, &image,
                    ));
                    stages.push((label, variant, image));
                }
            }
            if vulkan.len() > MAX_VARIANTS {
                lines.push(more_variants(fault, label, vulkan.len() - MAX_VARIANTS));
            }
        }
        let budget = if fault == 1 { HEX_BUDGET } else { 0 };
        let mut hex = HexBudget::new(budget);
        for (label, variant, image) in &stages {
            lines.extend(hex.code(fault, label, *variant, image));
        }
        lines.extend(hex.end(fault, budget));
        lines
    }
}

#[cfg(test)]
mod tests;
