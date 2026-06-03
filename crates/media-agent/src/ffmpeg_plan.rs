use crate::{ffmpeg_args::append_output_target, runtime::SuccessCheck};

#[derive(Debug, Clone)]
pub(crate) struct PublishOutput {
    pub(crate) target: String,
    pub(crate) format: String,
    pub(crate) muxer: String,
    pub(crate) success_check: SuccessCheck,
    pub(crate) output_args: Vec<String>,
}

pub(crate) fn append_publish_output_args(args: &mut Vec<String>, output: &PublishOutput) {
    append_output_target(
        args,
        &output.output_args,
        output.muxer.as_str(),
        output.target.as_str(),
    );
}
