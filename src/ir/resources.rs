use crate::ir::reference::{Origin, Reference};
use crate::parser::resource::ResourceValue;
use crate::parser::sub::{sub_parse_tree, SubValue};
use crate::specification::{spec, Complexity, Specification};
use crate::{CloudformationParseTree, TransmuteError};
use std::collections::HashMap;

// ResourceIr is the intermediate representation of a nested stack resource.
// It is slightly more refined than the ResourceValue, in some cases always resolving
// known types. It also decorates objects with the necessary information for a separate
// system to output all the necessary internal structures appropriately.
#[derive(Clone)]
pub enum ResourceIr {
    Null,
    Bool(bool),
    Number(i64),
    String(String),

    // Higher level resolutions
    Array(Complexity, Vec<ResourceIr>),
    Object(Complexity, HashMap<String, ResourceIr>),

    /// Rest is meta functions
    /// https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/intrinsic-function-reference-conditions.html#w2ab1c33c28c21c29
    If(String, Box<ResourceIr>, Box<ResourceIr>),
    Join(String, Vec<ResourceIr>),
    Ref(Reference),
    GetAtt(String, String),
    Sub(Vec<ResourceIr>),
    Map(Box<ResourceIr>, Box<ResourceIr>, Box<ResourceIr>),
}

/// ResourceTranslationInputs is a place to store all the intermediate recursion
/// for resource types.
#[derive(Clone, Debug)]
pub struct ResourceTranslationInputs<'t> {
    parse_tree: &'t CloudformationParseTree,
    specification: &'t Specification,
    complexity: Complexity,
    property_type: Option<&'t str>,
    resource_type: &'t str,
}

// ResourceInstruction is all the information needed to output a resource assignment.
pub struct ResourceInstruction {
    pub name: String,
    pub condition: Option<String>,
    pub resource_type: String,
    pub properties: HashMap<String, ResourceIr>,
}

pub fn translates_resources(parse_tree: &CloudformationParseTree) -> Vec<ResourceInstruction> {
    let spec = spec();
    let mut resource_instructions = Vec::new();
    for resource in parse_tree.resources.resources.iter() {
        let resource_spec = spec
            .resource_types
            .get(&resource.resource_type)
            .unwrap()
            .properties
            .as_ref();
        let mut props = HashMap::new();
        for (name, prop) in resource.properties.iter() {
            let property_rule = resource_spec.unwrap().get(name).unwrap();
            let complexity = property_rule.get_complexity();
            let property_type =
                Specification::full_property_name(&complexity, &resource.resource_type);
            let property_type = property_type.as_deref();
            let rt = ResourceTranslationInputs {
                parse_tree,
                specification: &spec,
                complexity: property_rule.get_complexity(),
                property_type,
                resource_type: &resource.resource_type,
            };

            let ir = translate_resource(prop, &rt).unwrap();
            props.insert(name.to_string(), ir);
        }

        resource_instructions.push(ResourceInstruction {
            name: resource.name.to_string(),
            resource_type: resource.resource_type.to_string(),
            condition: resource.condition.clone(),
            properties: props,
        });
    }
    resource_instructions
}

fn translate_resource(
    resource_value: &ResourceValue,
    resource_translator: &ResourceTranslationInputs,
) -> Result<ResourceIr, TransmuteError> {
    match resource_value {
        ResourceValue::Null => Ok(ResourceIr::Null),
        ResourceValue::Bool(b) => Ok(ResourceIr::Bool(*b)),
        ResourceValue::Number(n) => Ok(ResourceIr::Number(*n)),
        ResourceValue::String(s) => Ok(ResourceIr::String(s.to_string())),
        ResourceValue::Array(parse_resource_vec) => {
            let mut array_ir = Vec::new();
            for parse_resource in parse_resource_vec {
                let x = translate_resource(parse_resource, resource_translator)?;
                array_ir.push(x);
            }

            Ok(ResourceIr::Array(
                resource_translator.complexity.clone(),
                array_ir,
            ))
        }
        ResourceValue::Object(o) => {
            let mut new_hash = HashMap::new();
            for (s, rv) in o {
                let property_ir = match resource_translator.complexity {
                    Complexity::Simple(_) => translate_resource(rv, resource_translator)?,
                    Complexity::Complex(_) => {
                        // Update the rule with it's underlying property rule.
                        let mut new_rt = resource_translator.clone();
                        let rule = resource_translator
                            .specification
                            .property_types
                            .get(&resource_translator.property_type.unwrap().to_string())
                            .unwrap();
                        let properties = rule.properties.as_ref().unwrap();
                        let property_rule = properties.get(s).unwrap();
                        new_rt.complexity = property_rule.get_complexity();
                        let opt = Specification::full_property_name(
                            &property_rule.get_complexity(),
                            resource_translator.resource_type,
                        );
                        new_rt.property_type = opt.as_deref();
                        translate_resource(rv, &new_rt)?
                    }
                };

                new_hash.insert(s.to_string(), property_ir);
            }

            Ok(ResourceIr::Object(
                resource_translator.complexity.clone(),
                new_hash,
            ))
        }
        ResourceValue::Sub(arr) => {
            // Sub has two ways of being built: Either resolution via a bunch of objects
            // or everything is in the first sub element, and that's it.
            // just resolve the objects.
            let val = &arr[0];
            let val = match val {
                ResourceValue::String(x) => x,
                _ => return Err(TransmuteError::new("First value in sub must be a string")),
            };

            let mut excess_map = HashMap::new();
            if arr.len() > 1 {
                let mut iter = arr.iter();
                iter.next();

                for obj in iter {
                    match obj {
                        ResourceValue::Object(obj) => {
                            for (key, val) in obj.iter() {
                                let val_str = translate_resource(val, resource_translator)?;
                                excess_map.insert(key.to_string(), val_str);
                            }
                        }
                        _ => {
                            // these aren't possible, so panic
                            return Err(TransmuteError::new("Sub excess map must be an object"));
                        }
                    }
                }
            }
            let vars = sub_parse_tree(val.as_str())?;
            let r = vars
                .iter()
                .map(|x| match x {
                    SubValue::String(x) => ResourceIr::String(x.to_string()),
                    SubValue::Variable(x) => match excess_map.get(x) {
                        None => ResourceIr::Ref(find_ref(x, resource_translator.parse_tree)),
                        Some(x) => x.clone(),
                    },
                })
                .collect();
            Ok(ResourceIr::Sub(r))
        }
        ResourceValue::FindInMap(mapper, first, second) => {
            let mapper_str = translate_resource(mapper, resource_translator)?;
            let first_str = translate_resource(first, resource_translator)?;
            let second_str = translate_resource(second, resource_translator)?;
            Ok(ResourceIr::Map(
                Box::new(mapper_str),
                Box::new(first_str),
                Box::new(second_str),
            ))
        }
        ResourceValue::GetAtt(name, attribute) => {
            let name: &ResourceValue = name.as_ref();
            let attribute: &ResourceValue = attribute.as_ref();
            let resource_name = match name {
                ResourceValue::String(x) => x,
                _ => {
                    return Err(TransmuteError::new(
                        "Get attribute first element must be a string",
                    ))
                }
            };
            let attr_name = match attribute {
                ResourceValue::String(x) => x,
                _ => {
                    return Err(TransmuteError::new(
                        "Get attribute first element must be a string",
                    ))
                }
            };
            Ok(ResourceIr::GetAtt(
                resource_name.to_string(),
                attr_name.to_string(),
            ))
        }
        ResourceValue::If(bool_expr, true_expr, false_expr) => {
            let bool_expr = match bool_expr.as_ref() {
                ResourceValue::String(x) => x,
                &_ => {
                    return Err(TransmuteError::new(
                        "Resource value if statement truth must be a string",
                    ));
                }
            };
            let true_expr = translate_resource(true_expr, resource_translator)?;
            let false_expr = translate_resource(false_expr, resource_translator)?;

            Ok(ResourceIr::If(
                bool_expr.to_string(),
                Box::new(true_expr),
                Box::new(false_expr),
            ))
        }
        ResourceValue::Join(x) => {
            let sep = x.get(0).unwrap();

            let sep = match sep {
                ResourceValue::String(x) => x,
                _ => return Err(TransmuteError::new("Separator for join must be a string")),
            };

            let iterator = x.iter().skip(1);

            let mut irs = Vec::new();
            for rv in iterator {
                let resource_ir = translate_resource(rv, resource_translator)?;
                irs.push(resource_ir)
            }

            Ok(ResourceIr::Join(sep.to_string(), irs))
        }
        ResourceValue::Ref(x) => Ok(ResourceIr::Ref(find_ref(x, resource_translator.parse_tree))),
    }
}

fn find_ref(x: &str, parse_tree: &CloudformationParseTree) -> Reference {
    let opt_pseudo = Reference::match_pseudo_parameter(x);

    if let Some(pseudo) = opt_pseudo {
        return Reference::new(x, Origin::PseudoParameter(pseudo));
    }

    for (name, _) in parse_tree.parameters.params.iter() {
        if name == x {
            return Reference::new(x, Origin::Parameter);
        }
    }

    Reference::new(x, Origin::LogicalId)
}
