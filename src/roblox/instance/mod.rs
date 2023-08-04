use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    hash::{Hash, Hasher},
    sync::RwLock,
};

use mlua::prelude::*;
use once_cell::sync::Lazy;
use rbx_dom_weak::{
    types::{Attributes as DomAttributes, Ref as DomRef, Variant as DomValue},
    Instance as DomInstance, InstanceBuilder as DomInstanceBuilder, WeakDom,
};

use crate::roblox::shared::instance::{class_exists, class_is_a};

pub(crate) mod base;
pub(crate) mod data_model;
pub(crate) mod workspace;

const PROPERTY_NAME_ATTRIBUTES: &str = "Attributes";
const PROPERTY_NAME_TAGS: &str = "Tags";

static INTERNAL_DOM: Lazy<RwLock<WeakDom>> =
    Lazy::new(|| RwLock::new(WeakDom::new(DomInstanceBuilder::new("ROOT"))));

#[derive(Debug, Clone)]
pub struct Instance {
    pub(crate) dom_ref: DomRef,
    pub(crate) class_name: String,
}

impl Instance {
    /**
        Creates a new `Instance` from an existing dom object ref.

        Panics if the instance does not exist in the internal dom,
        or if the given dom object ref points to the dom root.
    */
    pub(crate) fn new(dom_ref: DomRef) -> Self {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let instance = dom
            .get_by_ref(dom_ref)
            .expect("Failed to find instance in document");

        if instance.referent() == dom.root_ref() {
            panic!("Instances can not be created from dom roots")
        }

        Self {
            dom_ref,
            class_name: instance.class.clone(),
        }
    }

    /**
        Creates a new `Instance` from a dom object ref, if the instance exists.

        Panics if the given dom object ref points to the dom root.
    */
    pub(crate) fn new_opt(dom_ref: DomRef) -> Option<Self> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        if let Some(instance) = dom.get_by_ref(dom_ref) {
            if instance.referent() == dom.root_ref() {
                panic!("Instances can not be created from dom roots")
            }

            Some(Self {
                dom_ref,
                class_name: instance.class.clone(),
            })
        } else {
            None
        }
    }

    /**
        Creates a new orphaned `Instance` with a given class name.

        An orphaned instance is an instance at the root of a weak dom.
    */
    pub(crate) fn new_orphaned(class_name: impl AsRef<str>) -> Self {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to document");

        let class_name = class_name.as_ref();

        let instance = DomInstanceBuilder::new(class_name.to_string());

        let dom_root = dom.root_ref();
        let dom_ref = dom.insert(dom_root, instance);

        Self {
            dom_ref,
            class_name: class_name.to_string(),
        }
    }

    /**
        Creates a new orphaned `Instance` by transferring
        it from an external weak dom to the internal one.

        An orphaned instance is an instance at the root of a weak dom.

        Panics if the given dom ref is the root dom ref of the external weak dom.
    */
    pub fn from_external_dom(external_dom: &mut WeakDom, external_dom_ref: DomRef) -> Self {
        {
            let mut dom = INTERNAL_DOM
                .try_write()
                .expect("Failed to get write access to document");
            let dom_root = dom.root_ref();

            external_dom.transfer(external_dom_ref, &mut dom, dom_root);
        }

        Self::new(external_dom_ref)
    }

    /**
        Clones an instance to an external weak dom.

        This will place the instance as a child of the
        root of the weak dom, and return its referent.
    */
    pub fn clone_into_external_dom(self, external_dom: &mut WeakDom) -> DomRef {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let cloned = dom.clone_into_external(self.dom_ref, external_dom);
        external_dom.transfer_within(cloned, external_dom.root_ref());

        cloned
    }

    /**
        Clones the instance and all of its descendants, and orphans it.

        To then save the new instance it must be re-parented,
        which matches the exact behavior of Roblox's instances.

        ### See Also
        * [`Clone`](https://create.roblox.com/docs/reference/engine/classes/Instance#Clone)
        on the Roblox Developer Hub
    */
    pub fn clone_instance(&self) -> Instance {
        // NOTE: We create a new scope here to avoid deadlocking since
        // our clone implementation must have exclusive write access
        let new_ref = {
            let mut dom = INTERNAL_DOM
                .try_write()
                .expect("Failed to get write access to document");

            dom.clone_within(self.dom_ref)
        };

        let new_inst = Self::new(new_ref);
        new_inst.set_parent(None);
        new_inst
    }

    /**
        Destroys the instance, removing it completely
        from the weak dom with no way of recovering it.

        All member methods will throw errors when called from lua and panic
        when called from rust after the instance has been destroyed.

        Returns `true` if destroyed successfully, `false` if already destroyed.

        ### See Also
        * [`Destroy`](https://create.roblox.com/docs/reference/engine/classes/Instance#Destroy)
        on the Roblox Developer Hub
    */
    pub fn destroy(&mut self) -> bool {
        if self.is_destroyed() {
            false
        } else {
            let mut dom = INTERNAL_DOM
                .try_write()
                .expect("Failed to get write access to document");

            dom.destroy(self.dom_ref);
            true
        }
    }

    fn is_destroyed(&self) -> bool {
        // NOTE: This property can not be cached since instance references
        // other than this one may have destroyed this one, and we don't
        // keep track of all current instance reference structs
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");
        dom.get_by_ref(self.dom_ref).is_none()
    }

    /**
        Destroys all child instances.

        ### See Also
        * [`Instance::Destroy`] for more info about what happens when an instance gets destroyed
        * [`ClearAllChildren`](https://create.roblox.com/docs/reference/engine/classes/Instance#ClearAllChildren)
        on the Roblox Developer Hub
    */
    pub fn clear_all_children(&mut self) {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to document");

        let instance = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document");

        let child_refs = instance.children().to_vec();
        for child_ref in child_refs {
            dom.destroy(child_ref);
        }
    }

    /**
        Checks if the instance matches or inherits a given class name.

        ### See Also
        * [`IsA`](https://create.roblox.com/docs/reference/engine/classes/Instance#IsA)
        on the Roblox Developer Hub
    */
    pub fn is_a(&self, class_name: impl AsRef<str>) -> bool {
        class_is_a(&self.class_name, class_name).unwrap_or(false)
    }

    /**
        Gets the class name of the instance.

        This will return the correct class name even if the instance has been destroyed.

        ### See Also
        * [`ClassName`](https://create.roblox.com/docs/reference/engine/classes/Instance#ClassName)
        on the Roblox Developer Hub
    */
    pub fn get_class_name(&self) -> &str {
        self.class_name.as_str()
    }

    /**
        Gets the name of the instance, if it exists.

        ### See Also
        * [`Name`](https://create.roblox.com/docs/reference/engine/classes/Instance#Name)
        on the Roblox Developer Hub
    */
    pub fn get_name(&self) -> String {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        dom.get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document")
            .name
            .clone()
    }

    /**
        Sets the name of the instance, if it exists.

        ### See Also
        * [`Name`](https://create.roblox.com/docs/reference/engine/classes/Instance#Name)
        on the Roblox Developer Hub
    */
    pub fn set_name(&self, name: impl Into<String>) {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to document");

        dom.get_by_ref_mut(self.dom_ref)
            .expect("Failed to find instance in document")
            .name = name.into()
    }

    /**
        Gets the parent of the instance, if it exists.

        ### See Also
        * [`Parent`](https://create.roblox.com/docs/reference/engine/classes/Instance#Parent)
        on the Roblox Developer Hub
    */
    pub fn get_parent(&self) -> Option<Instance> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let parent_ref = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document")
            .parent();

        if parent_ref == dom.root_ref() {
            None
        } else {
            Some(Self::new(parent_ref))
        }
    }

    /**
        Sets the parent of the instance, if it exists.

        If the provided parent is [`None`] the instance will become orphaned.

        An orphaned instance is an instance at the root of a weak dom.

        ### See Also
        * [`Parent`](https://create.roblox.com/docs/reference/engine/classes/Instance#Parent)
        on the Roblox Developer Hub
    */
    pub fn set_parent(&self, parent: Option<Instance>) {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to target document");

        let parent_ref = parent
            .map(|parent| parent.dom_ref)
            .unwrap_or_else(|| dom.root_ref());

        dom.transfer_within(self.dom_ref, parent_ref);
    }

    /**
        Gets a property for the instance, if it exists.
    */
    pub fn get_property(&self, name: impl AsRef<str>) -> Option<DomValue> {
        INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document")
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document")
            .properties
            .get(name.as_ref())
            .cloned()
    }

    /**
        Sets a property for the instance.

        Note that setting a property here will not fail even if the
        property does not actually exist for the instance class.
    */
    pub fn set_property(&self, name: impl AsRef<str>, value: DomValue) {
        INTERNAL_DOM
            .try_write()
            .expect("Failed to get read access to document")
            .get_by_ref_mut(self.dom_ref)
            .expect("Failed to find instance in document")
            .properties
            .insert(name.as_ref().to_string(), value);
    }

    /**
        Gets an attribute for the instance, if it exists.

        ### See Also
        * [`GetAttribute`](https://create.roblox.com/docs/reference/engine/classes/Instance#GetAttribute)
        on the Roblox Developer Hub
    */
    pub fn get_attribute(&self, name: impl AsRef<str>) -> Option<DomValue> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");
        let inst = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document");
        if let Some(DomValue::Attributes(attributes)) =
            inst.properties.get(PROPERTY_NAME_ATTRIBUTES)
        {
            attributes.get(name.as_ref()).cloned()
        } else {
            None
        }
    }

    /**
        Gets all known attributes for the instance.

        ### See Also
        * [`GetAttributes`](https://create.roblox.com/docs/reference/engine/classes/Instance#GetAttributes)
        on the Roblox Developer Hub
    */
    pub fn get_attributes(&self) -> BTreeMap<String, DomValue> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");
        let inst = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document");
        if let Some(DomValue::Attributes(attributes)) =
            inst.properties.get(PROPERTY_NAME_ATTRIBUTES)
        {
            attributes.clone().into_iter().collect()
        } else {
            BTreeMap::new()
        }
    }

    /**
        Sets an attribute for the instance.

        ### See Also
        * [`SetAttribute`](https://create.roblox.com/docs/reference/engine/classes/Instance#SetAttribute)
        on the Roblox Developer Hub
    */
    pub fn set_attribute(&self, name: impl AsRef<str>, value: DomValue) {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to document");
        let inst = dom
            .get_by_ref_mut(self.dom_ref)
            .expect("Failed to find instance in document");
        // NOTE: Attributes do not support integers, only floats
        let value = match value {
            DomValue::Int32(i) => DomValue::Float32(i as f32),
            DomValue::Int64(i) => DomValue::Float64(i as f64),
            value => value,
        };
        if let Some(DomValue::Attributes(attributes)) =
            inst.properties.get_mut(PROPERTY_NAME_ATTRIBUTES)
        {
            attributes.insert(name.as_ref().to_string(), value);
        } else {
            let mut attributes = DomAttributes::new();
            attributes.insert(name.as_ref().to_string(), value);
            inst.properties.insert(
                PROPERTY_NAME_ATTRIBUTES.to_string(),
                DomValue::Attributes(attributes),
            );
        }
    }

    /**
        Adds a tag to the instance.

        ### See Also
        * [`AddTag`](https://create.roblox.com/docs/reference/engine/classes/CollectionService#AddTag)
        on the Roblox Developer Hub
    */
    pub fn add_tag(&self, name: impl AsRef<str>) {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to document");
        let inst = dom
            .get_by_ref_mut(self.dom_ref)
            .expect("Failed to find instance in document");
        if let Some(DomValue::Tags(tags)) = inst.properties.get_mut(PROPERTY_NAME_TAGS) {
            tags.push(name.as_ref());
        } else {
            inst.properties.insert(
                PROPERTY_NAME_TAGS.to_string(),
                DomValue::Tags(vec![name.as_ref().to_string()].into()),
            );
        }
    }

    /**
        Gets all current tags for the instance.

        ### See Also
        * [`GetTags`](https://create.roblox.com/docs/reference/engine/classes/CollectionService#GetTags)
        on the Roblox Developer Hub
    */
    pub fn get_tags(&self) -> Vec<String> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");
        let inst = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document");
        if let Some(DomValue::Tags(tags)) = inst.properties.get(PROPERTY_NAME_TAGS) {
            tags.iter().map(ToString::to_string).collect()
        } else {
            Vec::new()
        }
    }

    /**
        Checks if the instance has a specific tag.

        ### See Also
        * [`HasTag`](https://create.roblox.com/docs/reference/engine/classes/CollectionService#HasTag)
        on the Roblox Developer Hub
    */
    pub fn has_tag(&self, name: impl AsRef<str>) -> bool {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");
        let inst = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document");
        if let Some(DomValue::Tags(tags)) = inst.properties.get(PROPERTY_NAME_TAGS) {
            let name = name.as_ref();
            tags.iter().any(|tag| tag == name)
        } else {
            false
        }
    }

    /**
        Removes a tag from the instance.

        ### See Also
        * [`RemoveTag`](https://create.roblox.com/docs/reference/engine/classes/CollectionService#RemoveTag)
        on the Roblox Developer Hub
    */
    pub fn remove_tag(&self, name: impl AsRef<str>) {
        let mut dom = INTERNAL_DOM
            .try_write()
            .expect("Failed to get write access to document");
        let inst = dom
            .get_by_ref_mut(self.dom_ref)
            .expect("Failed to find instance in document");
        if let Some(DomValue::Tags(tags)) = inst.properties.get_mut(PROPERTY_NAME_TAGS) {
            let name = name.as_ref();
            let mut new_tags = tags.iter().map(ToString::to_string).collect::<Vec<_>>();
            new_tags.retain(|tag| tag != name);
            inst.properties.insert(
                PROPERTY_NAME_TAGS.to_string(),
                DomValue::Tags(new_tags.into()),
            );
        }
    }

    /**
        Gets all of the current children of this `Instance`.

        Note that this is a somewhat expensive operation and that other
        operations using weak dom referents should be preferred if possible.

        ### See Also
        * [`GetChildren`](https://create.roblox.com/docs/reference/engine/classes/Instance#GetChildren)
        on the Roblox Developer Hub
    */
    pub fn get_children(&self) -> Vec<Instance> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let children = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document")
            .children()
            .to_vec();

        children.into_iter().map(Self::new).collect()
    }

    /**
        Gets all of the current descendants of this `Instance` using a breadth-first search.

        Note that this is a somewhat expensive operation and that other
        operations using weak dom referents should be preferred if possible.

        ### See Also
        * [`GetDescendants`](https://create.roblox.com/docs/reference/engine/classes/Instance#GetDescendants)
        on the Roblox Developer Hub
    */
    pub fn get_descendants(&self) -> Vec<Instance> {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let mut descendants = Vec::new();
        let mut queue = VecDeque::from_iter(
            dom.get_by_ref(self.dom_ref)
                .expect("Failed to find instance in document")
                .children(),
        );

        while let Some(queue_ref) = queue.pop_front() {
            descendants.push(*queue_ref);
            let queue_inst = dom.get_by_ref(*queue_ref).unwrap();
            for queue_ref_inner in queue_inst.children().iter().rev() {
                queue.push_back(queue_ref_inner);
            }
        }

        descendants.into_iter().map(Self::new).collect()
    }

    /**
        Gets the "full name" of this instance.

        This will be a path composed of instance names from the top-level
        ancestor of this instance down to itself, in the following format:

        `Ancestor.Child.Descendant.Instance`

        ### See Also
        * [`GetFullName`](https://create.roblox.com/docs/reference/engine/classes/Instance#GetFullName)
        on the Roblox Developer Hub
    */
    pub fn get_full_name(&self) -> String {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");
        let dom_root = dom.root_ref();

        let mut parts = Vec::new();
        let mut instance_ref = self.dom_ref;

        while let Some(instance) = dom.get_by_ref(instance_ref) {
            if instance_ref != dom_root && instance.class != data_model::CLASS_NAME {
                instance_ref = instance.parent();
                parts.push(instance.name.clone());
            } else {
                break;
            }
        }

        parts.reverse();
        parts.join(".")
    }

    /**
        Finds a child of the instance using the given predicate callback.

        ### See Also
        * [`FindFirstChild`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstChild) on the Roblox Developer Hub
        * [`FindFirstChildOfClass`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstChildOfClass) on the Roblox Developer Hub
        * [`FindFirstChildWhichIsA`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstChildWhichIsA) on the Roblox Developer Hub
    */
    pub fn find_child<F>(&self, predicate: F) -> Option<Instance>
    where
        F: Fn(&DomInstance) -> bool,
    {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let children = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document")
            .children()
            .to_vec();

        children.into_iter().find_map(|child_ref| {
            if let Some(child_inst) = dom.get_by_ref(child_ref) {
                if predicate(child_inst) {
                    Some(Self::new(child_ref))
                } else {
                    None
                }
            } else {
                None
            }
        })
    }

    /**
        Finds an ancestor of the instance using the given predicate callback.

        ### See Also
        * [`FindFirstAncestor`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstAncestor) on the Roblox Developer Hub
        * [`FindFirstAncestorOfClass`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstAncestorOfClass) on the Roblox Developer Hub
        * [`FindFirstAncestorWhichIsA`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstAncestorWhichIsA) on the Roblox Developer Hub
    */
    pub fn find_ancestor<F>(&self, predicate: F) -> Option<Instance>
    where
        F: Fn(&DomInstance) -> bool,
    {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let mut ancestor_ref = dom
            .get_by_ref(self.dom_ref)
            .expect("Failed to find instance in document")
            .parent();

        while let Some(ancestor) = dom.get_by_ref(ancestor_ref) {
            if predicate(ancestor) {
                return Some(Self::new(ancestor_ref));
            } else {
                ancestor_ref = ancestor.parent();
            }
        }

        None
    }

    /**
        Finds a descendant of the instance using the given
        predicate callback and a breadth-first search.

        ### See Also
        * [`FindFirstDescendant`](https://create.roblox.com/docs/reference/engine/classes/Instance#FindFirstDescendant)
        on the Roblox Developer Hub
    */
    pub fn find_descendant<F>(&self, predicate: F) -> Option<Instance>
    where
        F: Fn(&DomInstance) -> bool,
    {
        let dom = INTERNAL_DOM
            .try_read()
            .expect("Failed to get read access to document");

        let mut queue = VecDeque::from_iter(
            dom.get_by_ref(self.dom_ref)
                .expect("Failed to find instance in document")
                .children(),
        );

        while let Some(queue_item) = queue
            .pop_front()
            .and_then(|queue_ref| dom.get_by_ref(*queue_ref))
        {
            if predicate(queue_item) {
                return Some(Self::new(queue_item.referent()));
            } else {
                queue.extend(queue_item.children())
            }
        }

        None
    }
}

impl Instance {
    pub(crate) fn make_table(lua: &Lua, datatype_table: &LuaTable) -> LuaResult<()> {
        datatype_table.set(
            "new",
            lua.create_function(|lua, class_name: String| {
                if class_exists(&class_name) {
                    Instance::new_orphaned(class_name).into_lua(lua)
                } else {
                    Err(LuaError::RuntimeError(format!(
                        "Failed to create Instance - '{}' is not a valid class name",
                        class_name
                    )))
                }
            })?,
        )
    }
}

/*
    Here we add inheritance-like behavior for instances by creating
    fields that are restricted to specific classnames / base classes

    Note that we should try to be conservative with how many classes
    and methods we support here - we should only implement methods that
    are necessary for modifying the dom and / or having ergonomic access
    to the dom, not try to replicate Roblox engine behavior of instances
*/
impl LuaUserData for Instance {
    fn add_fields<'lua, F: LuaUserDataFields<'lua, Self>>(fields: &mut F) {
        data_model::add_fields(fields);
        workspace::add_fields(fields);
    }

    fn add_methods<'lua, M: LuaUserDataMethods<'lua, Self>>(methods: &mut M) {
        base::add_methods(methods);
        data_model::add_methods(methods);
    }
}

impl Hash for Instance {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.dom_ref.hash(state)
    }
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            if self.is_destroyed() {
                "<<DESTROYED>>".to_string()
            } else {
                self.get_name()
            }
        )
    }
}

impl PartialEq for Instance {
    fn eq(&self, other: &Self) -> bool {
        self.dom_ref == other.dom_ref
    }
}

impl From<Instance> for DomRef {
    fn from(value: Instance) -> Self {
        value.dom_ref
    }
}
