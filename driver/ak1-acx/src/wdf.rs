use wdk_sys::{_WDF_EXECUTION_LEVEL, _WDF_SYNCHRONIZATION_SCOPE, WDF_OBJECT_ATTRIBUTES, WDFOBJECT};

/// Equivalent of `WDF_OBJECT_ATTRIBUTES_INIT` with a parent object.
pub fn object_attributes(parent: WDFOBJECT) -> WDF_OBJECT_ATTRIBUTES {
    WDF_OBJECT_ATTRIBUTES {
        Size: size_of::<WDF_OBJECT_ATTRIBUTES>() as u32,
        EvtCleanupCallback: None,
        EvtDestroyCallback: None,
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ParentObject: parent,
        ContextSizeOverride: 0,
        ContextTypeInfo: core::ptr::null(),
    }
}
