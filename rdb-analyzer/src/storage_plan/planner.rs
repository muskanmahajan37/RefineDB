use std::{
  collections::{BTreeMap, HashMap, HashSet},
  sync::Arc,
};

use anyhow::Result;
use rand::RngCore;

use crate::schema::compile::{CompiledSchema, FieldAnnotation, FieldType};

use super::{StorageKey, StorageNode, StorageNodeKey, StoragePlan};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlannerError {
  #[error("missing type: {0}")]
  MissingType(Arc<str>),
}

struct PlanState<'a> {
  subspaces_assigned: HashMap<usize, StorageKey>,
  old_schema: &'a CompiledSchema,
}

#[derive(Default)]
struct SubspaceState {
  fields_in_stack: HashSet<usize>,
}

/// A point on the old tree.
#[derive(Copy, Clone)]
struct OldTreePoint<'a> {
  name: &'a str,
  ty: &'a FieldType,
  annotations: &'a [FieldAnnotation],
  node: &'a StorageNode,
}

impl<'a> OldTreePoint<'a> {
  fn reduce_optional(mut self) -> Self {
    if let FieldType::Optional(x) = self.ty {
      log::trace!(
        "optional field `{}` of type `{}` reduced to `{}`.",
        self.name,
        self.ty,
        x
      );
      self.ty = &**x;
    } else {
      log::info!("field `{}` was mandatory but now optional", self.name);
    }

    self
  }

  fn reduce_set(mut self) -> Option<Self> {
    if let FieldType::Set(x) = self.ty {
      log::trace!(
        "set `{}` of type `{}` reduced to `{}`.",
        self.name,
        self.ty,
        x
      );
      self.ty = &**x;
      match &self.node.key {
        Some(StorageNodeKey::Set(x)) => {
          self.node = &**x;
          Some(self)
        }
        _ => {
          log::error!("inconsistency detected: a storage node for the `set` type does not have a `StorageNodeKey::Set` storage key. dropping field. node: {:?}", self.node);
          None
        }
      }
    } else {
      log::warn!(
        "field `{}` becomes a set - previous value will not be preserved",
        self.name
      );
      None
    }
  }

  fn validate_type(
    self,
    expected_ty: &FieldType,
    expected_annotations: &[FieldAnnotation],
  ) -> Option<Self> {
    if self.ty != expected_ty {
      let mut mandatory_to_optional = false;
      if let FieldType::Optional(x) = expected_ty {
        if &**x == self.ty {
          mandatory_to_optional = true;
        }
      }
      if !mandatory_to_optional {
        log::warn!(
          "field `{}` had type `{}` but the new type is `{}` - previous value will not be preserved",
          self.name,
          self.ty,
          expected_ty,
        );
      }
      return None;
    }

    if self.annotations.iter().find(|x| x.is_packed()).is_some()
      && !expected_annotations
        .iter()
        .find(|x| x.is_packed())
        .is_some()
    {
      log::warn!(
        "field `{}` was not packed but is packed now - previous value will not be preserved",
        self.name
      );
      return None;
    }

    if !self.annotations.iter().find(|x| x.is_packed()).is_some()
      && expected_annotations
        .iter()
        .find(|x| x.is_packed())
        .is_some()
    {
      log::warn!(
        "field `{}` was packed but is not packed now - previous value will not be preserved",
        self.name
      );
      return None;
    }
    Some(self)
  }

  fn storage_key(&self) -> Option<StorageKey> {
    if let Some(StorageNodeKey::Const(x)) = self.node.key {
      Some(x)
    } else {
      log::warn!(
        "requesting non-present storage key of field `{}` (type `{}`) - previous value will not be preserved",
        self.name,
        self.ty,
      );
      None
    }
  }

  fn resolve_subfield(&self, plan_st: &PlanState<'a>, name: &str) -> Option<Self> {
    let child_node = match self.node.children.get(name) {
      Some(x) => x,
      None => {
        log::info!(
          "subfield `{}` of type `{}` does not exist in the old plan - creating. {:?}",
          name,
          self.ty,
          self.node.children
        );
        return None;
      }
    };
    log::trace!(
      "subfield `{}` of type `{}` resolved to `{:?}`.",
      name,
      self.ty,
      child_node
    );
    let ty = match self.ty {
      FieldType::Named(type_name) => match plan_st.old_schema.types.get(type_name) {
        Some(x) => x,
        None => {
          log::warn!(
            "subfield `{}`'s type, `{}`, does not exist in the old schema",
            name,
            self.ty
          );
          return None;
        }
      },
      _ => {
        log::warn!(
          "cannot get subfield `{}` on an unnamed type `{}`",
          name,
          self.ty
        );
        return None;
      }
    };
    let (child_name, child_ty) = match ty.fields.get_key_value(name) {
      Some(x) => x,
      None => {
        log::warn!(
          "subfield `{}` exists in the old plan but not in the old schema",
          name
        );
        return None;
      }
    };
    Some(Self {
      name: &**child_name,
      ty: &child_ty.0,
      annotations: child_ty.1.as_slice(),
      node: child_node,
    })
  }
}

pub fn generate_plan_for_schema(
  old_plan: &StoragePlan,
  old_schema: &CompiledSchema,
  schema: &CompiledSchema,
) -> Result<StoragePlan> {
  let mut plan_st = PlanState {
    subspaces_assigned: HashMap::new(),
    old_schema,
  };
  let mut plan = StoragePlan {
    nodes: BTreeMap::new(),
  };

  for (export_name, export_field) in &schema.exports {
    // Retrieve the point in the old tree where the export possibly exists.
    let old_point = old_schema
      .exports
      .get(&**export_name)
      .and_then(|ty| old_plan.nodes.get(&**export_name).map(|x| (ty, x)))
      .map(|(ty, node)| OldTreePoint {
        name: &**export_name,
        ty,
        annotations: &[],
        node,
      })
      .and_then(|x| x.validate_type(export_field, &[]));

    // Here we don't generate using `generate_subspace`, because root nodes might be a `set`
    // but `generate_subspace` is only supposed to be used on user-defined named types.
    let node = generate_field(
      &mut plan_st,
      &mut SubspaceState::default(),
      schema,
      export_field,
      &[],
      old_point,
    )?;
    plan.nodes.insert(export_name.clone(), node);
  }
  Ok(plan)
}

/// The `old_point` parameter must be validated to match `field` before being passed to this function.
fn generate_subspace(
  plan_st: &mut PlanState,
  schema: &CompiledSchema,
  field: &FieldType,
  annotations: &[FieldAnnotation],
  old_point: Option<OldTreePoint>,
) -> Result<StorageNode> {
  let key = field_type_key(field);

  // If this subspace is already generated, return a `subspace_reference` leaf node...
  if let Some(storage_key) = plan_st.subspaces_assigned.get(&key) {
    return Ok(StorageNode {
      key: Some(StorageNodeKey::Const(*storage_key)),
      subspace_reference: true,
      packed: false,
      children: BTreeMap::new(),
    });
  }

  // Otherwise, generate the subspace.
  let storage_key = old_point
    .and_then(|x| x.storage_key())
    .unwrap_or_else(|| rand_storage_key());
  plan_st.subspaces_assigned.insert(key, storage_key);

  let mut subspace_st = SubspaceState {
    fields_in_stack: HashSet::new(),
  };
  let res = generate_field(
    plan_st,
    &mut subspace_st,
    schema,
    field,
    annotations,
    old_point,
  );
  plan_st.subspaces_assigned.remove(&key);

  // Tag result with subspace key
  let mut res = res?;
  res.key = Some(StorageNodeKey::Const(storage_key));

  Ok(res)
}

/// The `old_point` parameter must be validated to match `field` before being passed to this function.
fn generate_field(
  plan_st: &mut PlanState,
  subspace_st: &mut SubspaceState,
  schema: &CompiledSchema,
  field: &FieldType,
  annotations: &[FieldAnnotation],
  old_point: Option<OldTreePoint>,
) -> Result<StorageNode> {
  match field {
    FieldType::Optional(x) => {
      // Push down optional
      generate_field(
        plan_st,
        subspace_st,
        schema,
        x,
        annotations,
        old_point.map(|x| x.reduce_optional()),
      )
    }
    FieldType::Named(x) => {
      // This type has children. Push down.

      // For packed types, don't go down further...
      if annotations.iter().find(|x| x.is_packed()).is_some() {
        return Ok(StorageNode {
          key: Some(StorageNodeKey::Const(
            old_point
              .and_then(|x| x.storage_key())
              .unwrap_or_else(|| rand_storage_key()),
          )),
          subspace_reference: false,
          packed: true,
          children: BTreeMap::new(),
        });
      }

      // First, check whether we are resolving something recursively...
      if subspace_st.fields_in_stack.contains(&field_type_key(field)) {
        return generate_subspace(plan_st, schema, field, annotations, old_point);
      }

      let ty = schema
        .types
        .get(x)
        .ok_or_else(|| PlannerError::MissingType(x.clone()))?;

      // Push the current state.
      let key = field_type_key(field);
      subspace_st.fields_in_stack.insert(key);

      let mut children: BTreeMap<Arc<str>, StorageNode> = BTreeMap::new();

      // Iterate over the fields & recursively generate storage nodes.
      for subfield in &ty.fields {
        let subfield_old_point = old_point
          .and_then(|x| x.resolve_subfield(plan_st, &subfield.0))
          .and_then(|x| x.validate_type(&subfield.1 .0, &subfield.1 .1));
        match generate_field(
          plan_st,
          subspace_st,
          schema,
          &subfield.1 .0,
          &subfield.1 .1,
          subfield_old_point,
        ) {
          Ok(x) => {
            children.insert(subfield.0.clone(), x);
          }
          Err(e) => {
            subspace_st.fields_in_stack.remove(&key);
            return Err(e);
          }
        }
      }
      subspace_st.fields_in_stack.remove(&key);

      Ok(StorageNode {
        key: None,
        subspace_reference: false,
        packed: false,
        children,
      })
    }
    FieldType::Primitive(_) => {
      // This is a primitive type (leaf node).
      Ok(StorageNode {
        key: Some(StorageNodeKey::Const(
          old_point
            .and_then(|x| x.storage_key())
            .unwrap_or_else(|| rand_storage_key()),
        )),
        subspace_reference: false,
        packed: false,
        children: BTreeMap::new(),
      })
    }
    FieldType::Set(x) => {
      // This is a set with dynamic node key.
      let inner = generate_field(
        plan_st,
        subspace_st,
        schema,
        x,
        annotations,
        old_point
          .and_then(|x| x.reduce_set())
          .and_then(|y| y.validate_type(x, annotations)),
      )?;
      Ok(StorageNode {
        key: Some(StorageNodeKey::Set(Box::new(inner))),
        subspace_reference: false,
        packed: false,
        children: BTreeMap::new(),
      })
    }
  }
}

fn field_type_key(x: &FieldType) -> usize {
  x as *const _ as usize
}

fn rand_storage_key() -> StorageKey {
  let mut ret = [0u8; 16];
  rand::thread_rng().fill_bytes(&mut ret);
  ret
}
