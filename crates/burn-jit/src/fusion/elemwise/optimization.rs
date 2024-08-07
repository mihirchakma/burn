use std::sync::Arc;

use super::{
    kernel::ElementWiseKernelFactory, tune::ElementWiseAutotuneOperationSet,
    FusionElemWiseAutotuneKey,
};
use crate::{
    fusion::{kernel::FusionKernel, tracing::Trace, JitFusionHandle},
    tune_key::JitAutotuneKey,
    JitRuntime, JitTuneId,
};
use burn_common::id::IdGenerator;
use burn_fusion::stream::Context;
use cubecl::ir::CubeDim;
use cubecl::{
    client::ComputeClient,
    tune::{local_tuner, LocalTuner},
};
use serde::{Deserialize, Serialize};

#[derive(new)]
pub struct ElementWise<R: JitRuntime, Phase = ExecutionPhase<R>> {
    pub(super) trace: Trace,
    pub(super) num_operations: usize,
    pub(super) device: R::Device,
    pub(super) phase: Phase,
}

/// Phase where the kernel should be compiled.
pub struct CompilationPhase;

/// Phase where the kernel should be executed.
#[derive(new)]
pub struct ExecutionPhase<R: JitRuntime> {
    /// Kernel set with default cube size.
    pub(super) kernel_factory_1: ElementWiseKernelFactory<R>,
    /// Kernel set with custom cube size.
    pub(super) kernel_factory_2: ElementWiseKernelFactory<R>,
}

#[derive(new, Serialize, Deserialize)]
pub struct ElementWiseState {
    trace: Trace,
    num_operations: usize,
}

impl<R: JitRuntime> ElementWise<R, CompilationPhase> {
    pub(crate) fn compile(self) -> ElementWise<R, ExecutionPhase<R>> {
        let info = Arc::new(self.trace.compiling());

        let kernel_factory_1 = ElementWiseKernelFactory::new(
            IdGenerator::generate(),
            info.clone(),
            CubeDim::default(),
        );
        let kernel_factory_2 =
            ElementWiseKernelFactory::new(IdGenerator::generate(), info, CubeDim::new(16, 16, 1));

        ElementWise {
            trace: self.trace,
            device: self.device,
            phase: ExecutionPhase::new(kernel_factory_1, kernel_factory_2),
            num_operations: self.num_operations,
        }
    }
}

impl<R: JitRuntime> ElementWise<R, ExecutionPhase<R>> {
    pub(crate) fn execute(&mut self, context: &mut Context<'_, JitFusionHandle<R>>) {
        let client = R::client(&self.device);

        let key = JitAutotuneKey::FusionElemWise(FusionElemWiseAutotuneKey::new(
            self.num_operations,
            self.autotune_shape(context),
        ));

        let id = JitTuneId::new::<R>(&self.device);

        static TUNER: LocalTuner<JitAutotuneKey, JitTuneId> = local_tuner!();

        if let Some(index) = TUNER.autotune_result(&id, &key) {
            self.run_kernel(context, client, index)
        } else {
            self.run_autotune(context, client, id, key, &TUNER)
        }
    }

    fn run_kernel(
        &mut self,
        context: &mut Context<'_, JitFusionHandle<R>>,
        client: ComputeClient<R::Server, R::Channel>,
        fastest_set_index: usize,
    ) {
        let info = self.trace.running();
        let kernel_set = match fastest_set_index {
            0 => &self.phase.kernel_factory_1,
            1 => &self.phase.kernel_factory_2,
            _ => panic!("Should be 0 or 1, got {fastest_set_index}"),
        };

        let kernel = FusionKernel::create(
            kernel_set,
            &info,
            context,
            self.device.clone(),
            client,
            true,
        );

        kernel.execute();
    }

    fn run_autotune(
        &mut self,
        context: &mut Context<'_, JitFusionHandle<R>>,
        client: ComputeClient<R::Server, R::Channel>,
        id: JitTuneId,
        key: JitAutotuneKey,
        tuner: &LocalTuner<JitAutotuneKey, JitTuneId>,
    ) {
        let info = self.trace.running();

        let kernel_1 = FusionKernel::create(
            &self.phase.kernel_factory_1,
            &info,
            context,
            self.device.clone(),
            client.clone(),
            false,
        );
        let kernel_2 = FusionKernel::create(
            &self.phase.kernel_factory_2,
            &info,
            context,
            self.device.clone(),
            client.clone(),
            false,
        );
        let kernel_default = FusionKernel::create(
            &self.phase.kernel_factory_1,
            &info,
            context,
            self.device.clone(),
            client.clone(),
            false,
        );

        tuner.execute(
            &id,
            &client,
            Box::new(ElementWiseAutotuneOperationSet::new(
                key,
                kernel_1.into(),
                kernel_2.into(),
                kernel_default.into(),
            )),
        );
    }

    pub(crate) fn len(&self) -> usize {
        self.num_operations
    }

    /// The first output is chosen when possible, otherwise the first input is chosen.
    pub(crate) fn autotune_shape<'a>(
        &self,
        context: &mut Context<'a, JitFusionHandle<R>>,
    ) -> &'a [usize] {
        let info = self.trace.running();

        if let Some(tensor) = info.outputs.first() {
            let tensor = context.tensors.get(&tensor.id).unwrap();
            return &tensor.shape;
        }

        if let Some(tensor) = info.inputs.first() {
            let tensor = context.tensors.get(&tensor.id).unwrap();
            return &tensor.shape;
        }

        &[]
    }

    pub(crate) fn from_state(device: &R::Device, state: ElementWiseState) -> Self {
        // We don't save the compiled kernel structs since it's quick to compile and the output is
        // very large.
        //
        // It is still unclear if the deserialization would be that much faster than
        // simply recompiling it.
        ElementWise {
            trace: state.trace,
            device: device.clone(),
            phase: CompilationPhase,
            num_operations: state.num_operations,
        }
        .compile()
    }

    pub(crate) fn to_state(&self) -> ElementWiseState {
        ElementWiseState {
            trace: self.trace.clone(),
            num_operations: self.num_operations,
        }
    }
}
