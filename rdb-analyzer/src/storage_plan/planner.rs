use std::{
  collections::{BTreeMap, HashMap, HashSet},
  sync::Arc,
  time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use byteorder::{BigEndian, ByteOrder};
use rand::RngCore;

use crate::schema::compile::{CompiledSchema, FieldAnnotation, FieldAnnotationList, FieldType};

use super::{StorageKey, StorageNode, StoragePlan};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlannerError {
  #[error("missing type: {0}")]
  MissingType(Arc<str>),
}

struct PlanState<'a> {
  old_schema: &'a CompiledSchema,
  used_storage_keys: HashSet<StorageKey>,
  recursive_types: HashSet<usize>,
  fields_in_stack: HashMap<usize, StorageKey>,
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
      match &self.node.set {
        Some(x) => {
          self.node = &**x;
          Some(self)
        }
        None => {
          log::error!("inconsistency detected: a storage node for the `set` type does not have an element node. dropping field. node: {:?}", self.node);
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

  fn resolve_subfield(&self, plan_st: &PlanState<'a>, altnames: &[&str]) -> Option<Self> {
    let (name, child_node) = match altnames
      .iter()
      .find_map(|x| self.node.children.get(*x).map(|y| (*x, y)))
    {
      Some(x) => x,
      None => {
        log::info!(
          "none of the subfields `{:?}` exist in the old version of the type `{}` - creating.",
          altnames,
          self.ty,
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
  // Collect recursive types
  let mut recursive_types: HashSet<usize> = HashSet::new();
  for (_, export_field) in &schema.exports {
    collect_recursive_types(
      export_field,
      schema,
      &mut HashSet::new(),
      &mut recursive_types,
    )?;
  }
  log::debug!(
    "collected {} recursive types reachable from exports",
    recursive_types.len()
  );

  let mut plan_st = PlanState {
    old_schema,
    used_storage_keys: HashSet::new(),
    recursive_types,
    fields_in_stack: HashMap::new(),
  };

  // Deduplicate also against storage keys used in the previous plan.
  //
  // This is not strictly effective because we may have more than one historic schema
  // versions, but in that case the storage key generation mechanism should be enough
  // to prevent duplicates. (unless we generate a lot of schemas within a single
  // millisecond?)
  for (_, node) in &old_plan.nodes {
    collect_storage_keys(node, &mut plan_st.used_storage_keys);
  }
  log::debug!(
    "collected {} storage keys from old plan",
    plan_st.used_storage_keys.len()
  );
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

    let node = generate_field(&mut plan_st, schema, export_field, &[], old_point)?;
    plan.nodes.insert(export_name.clone(), node);
  }
  Ok(plan)
}

/// The `old_point` parameter must be validated to match `field` before being passed to this function.
fn generate_field(
  plan_st: &mut PlanState,
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
          key: old_point
            .map(|x| x.node.key)
            .unwrap_or_else(|| rand_storage_key(plan_st)),
          flattened: false,
          subspace_reference: false,
          packed: true,
          set: None,
          children: BTreeMap::new(),
        });
      }

      // First, check whether we are resolving something recursively...
      if let Some(key) = plan_st.fields_in_stack.get(&field_type_key(field)) {
        return Ok(StorageNode {
          key: *key,
          flattened: false,
          subspace_reference: true,
          packed: false,
          set: None,
          children: BTreeMap::new(),
        });
      }

      let ty = schema
        .types
        .get(x)
        .ok_or_else(|| PlannerError::MissingType(x.clone()))?;

      // Push the current state.
      let key = field_type_key(field);
      let flattened;
      let storage_key = old_point
        .map(|x| x.node.key)
        .unwrap_or_else(|| rand_storage_key(plan_st));

      // Recursive types cannot be flattened
      if plan_st.recursive_types.contains(&key) {
        flattened = false;
        plan_st.fields_in_stack.insert(key, storage_key);
      } else {
        flattened = true;
      }

      let mut children: BTreeMap<Arc<str>, StorageNode> = BTreeMap::new();

      // Iterate over the fields & recursively generate storage nodes.
      for subfield in &ty.fields {
        let (_, annotations) = subfield.1;
        let mut altnames = vec![&**subfield.0];
        for ann in annotations {
          match ann {
            FieldAnnotation::RenameFrom(x) => {
              altnames.push(x.as_str());
            }
            _ => {}
          }
        }

        let subfield_old_point = old_point
          .and_then(|x| x.resolve_subfield(plan_st, &altnames))
          .and_then(|x| x.validate_type(&subfield.1 .0, &subfield.1 .1));
        match generate_field(
          plan_st,
          schema,
          &subfield.1 .0,
          &subfield.1 .1,
          subfield_old_point,
        ) {
          Ok(x) => {
            children.insert(subfield.0.clone(), x);
          }
          Err(e) => {
            return Err(e);
          }
        }
      }
      if plan_st.recursive_types.contains(&key) {
        plan_st.fields_in_stack.remove(&key);
      }

      Ok(StorageNode {
        key: storage_key,
        flattened,
        subspace_reference: false,
        packed: false,
        set: None,
        children,
      })
    }
    FieldType::Primitive(_) => {
      // This is a primitive type (leaf node).
      Ok(StorageNode {
        key: old_point
          .map(|x| x.node.key)
          .unwrap_or_else(|| rand_storage_key(plan_st)),
        flattened: false,
        subspace_reference: false,
        packed: false,
        set: None,
        children: BTreeMap::new(),
      })
    }
    FieldType::Set(x) => {
      // This is a set with dynamic node key.
      let inner = generate_field(
        plan_st,
        schema,
        x,
        annotations,
        old_point
          .and_then(|x| x.reduce_set())
          .and_then(|y| y.validate_type(x, annotations)),
      )?;
      Ok(StorageNode {
        key: old_point
          .map(|x| x.node.key)
          .unwrap_or_else(|| rand_storage_key(plan_st)),
        flattened: false,
        subspace_reference: false,
        packed: false,
        set: Some(Box::new(inner)),
        children: BTreeMap::new(),
      })
    }
  }
}

fn field_type_key(x: &FieldType) -> usize {
  x as *const _ as usize
}

fn rand_storage_key(st: &mut PlanState) -> StorageKey {
  loop {
    let now = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    let mut timebuf = [0u8; 8];
    BigEndian::write_u64(&mut timebuf, now);

    assert_eq!(timebuf[0], 0);
    assert_eq!(timebuf[1], 0);

    let mut ret = [0u8; 12];
    ret[..6].copy_from_slice(&timebuf[2..]);
    rand::thread_rng().fill_bytes(&mut ret[6..]);

    if st.used_storage_keys.insert(ret) {
      break ret;
    }
  }
}

fn collect_storage_keys(node: &StorageNode, sink: &mut HashSet<StorageKey>) {
  sink.insert(node.key);
  if let Some(x) = &node.set {
    collect_storage_keys(x, sink);
  }
  for (_, child) in &node.children {
    collect_storage_keys(child, sink);
  }
}

fn collect_recursive_types(
  ty: &FieldType,
  schema: &CompiledSchema,
  state: &mut HashSet<usize>,
  sink: &mut HashSet<usize>,
) -> Result<()> {
  match ty {
    FieldType::Optional(x) => collect_recursive_types(x, schema, state, sink),
    FieldType::Set(x) => collect_recursive_types(x, schema, state, sink),
    FieldType::Primitive(_) => Ok(()),
    FieldType::Named(x) => {
      let type_key = field_type_key(ty);

      // if a cycle is detected...
      if state.insert(type_key) == false {
        sink.insert(type_key);
        return Ok(());
      }

      let specialized_ty = schema
        .types
        .get(x)
        .ok_or_else(|| PlannerError::MissingType(x.clone()))?;

      for (_, (field, annotations)) in &specialized_ty.fields {
        // Skip packed fields
        if annotations.as_slice().is_packed() {
          continue;
        }

        collect_recursive_types(field, schema, state, sink)?;
      }

      state.remove(&type_key);
      Ok(())
    }
  }
}
