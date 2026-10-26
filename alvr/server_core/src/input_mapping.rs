use alvr_common::*;
use alvr_packets::{ButtonEntry, ButtonValue};
use alvr_session::{
    AutomaticButtonMappingConfig, BinaryToScalarStates, ButtonBindingTarget, ButtonMappingType,
    ControllersEmulationMode, HysteresisThreshold, Range,
};
use std::collections::{HashMap, HashSet};

pub fn registered_button_set(
    controllers_emulation_mode: &ControllersEmulationMode,
) -> HashSet<u64> {
    match &controllers_emulation_mode {
        ControllersEmulationMode::RiftSTouch
        | ControllersEmulationMode::Quest1Touch
        | ControllersEmulationMode::Quest2Touch
        | ControllersEmulationMode::Quest3Plus
        | ControllersEmulationMode::QuestPro => CONTROLLER_PROFILE_INFO
            .get(&QUEST_CONTROLLER_PROFILE_ID)
            .unwrap()
            .button_set
            .clone(),
        ControllersEmulationMode::Pico4 => CONTROLLER_PROFILE_INFO
            .get(&PICO4_CONTROLLER_PROFILE_ID)
            .unwrap()
            .button_set
            .clone(),
        ControllersEmulationMode::PSVR2Sense => CONTROLLER_PROFILE_INFO
            .get(&PSVR2_CONTROLLER_PROFILE_ID)
            .unwrap()
            .button_set
            .clone(),
        ControllersEmulationMode::ValveIndex => CONTROLLER_PROFILE_INFO
            .get(&INDEX_CONTROLLER_PROFILE_ID)
            .unwrap()
            .button_set
            .clone(),
        ControllersEmulationMode::ViveWand => CONTROLLER_PROFILE_INFO
            .get(&VIVE_CONTROLLER_PROFILE_ID)
            .unwrap()
            .button_set
            .clone(),
        ControllersEmulationMode::ViveTracker => HashSet::new(),
        ControllersEmulationMode::Custom { button_set, .. } => button_set
            .iter()
            .map(|b| alvr_common::hash_string(b))
            .collect(),
    }
}

pub struct BindingTarget {
    destination: u64,
    mapping_type: ButtonMappingType,
    binary_conditions: Vec<u64>,
}

// Inputs relative to the same physical button
#[derive(Clone, Copy)]
pub struct ButtonInputs {
    click: Option<u64>,
    touch: Option<u64>,
    value: Option<u64>,
    force: Option<u64>,
}

fn click(click: u64) -> ButtonInputs {
    ButtonInputs {
        click: Some(click),
        touch: None,
        value: None,
        force: None,
    }
}

fn ct(set: &HashSet<u64>, click: u64, touch: u64) -> ButtonInputs {
    ButtonInputs {
        click: Some(click),
        touch: set.contains(&touch).then_some(touch),
        value: None,
        force: None,
    }
}

fn value(value: u64) -> ButtonInputs {
    ButtonInputs {
        click: None,
        touch: None,
        value: Some(value),
        force: None,
    }
}

fn ctv(set: &HashSet<u64>, click: u64, touch: u64, value: u64) -> ButtonInputs {
    ButtonInputs {
        click: set.contains(&click).then_some(click),
        touch: set.contains(&touch).then_some(touch),
        value: set.contains(&value).then_some(value),
        force: None,
    }
}

fn ctvf(set: &HashSet<u64>, click: u64, touch: u64, value: u64, force: u64) -> ButtonInputs {
    ButtonInputs {
        click: set.contains(&click).then_some(click),
        touch: set.contains(&touch).then_some(touch),
        value: set.contains(&value).then_some(value),
        force: set.contains(&force).then_some(force),
    }
}

fn passthrough(target: u64) -> BindingTarget {
    BindingTarget {
        destination: target,
        mapping_type: ButtonMappingType::Passthrough,
        binary_conditions: vec![],
    }
}

fn binary_to_scalar(target: u64, map: BinaryToScalarStates) -> BindingTarget {
    BindingTarget {
        destination: target,
        mapping_type: ButtonMappingType::BinaryToScalar(map),
        binary_conditions: vec![],
    }
}

fn hysteresis_threshold(target: u64, map: HysteresisThreshold) -> BindingTarget {
    BindingTarget {
        destination: target,
        mapping_type: ButtonMappingType::HysteresisThreshold(map),
        binary_conditions: vec![],
    }
}

fn remap(target: u64, map: Range) -> BindingTarget {
    BindingTarget {
        destination: target,
        mapping_type: ButtonMappingType::Remap(map),
        binary_conditions: vec![],
    }
}

// Map two buttons with eterogeneous inputs
fn map_button_pair_automatic(
    source: ButtonInputs,
    destination: ButtonInputs,
    config: &AutomaticButtonMappingConfig,
) -> impl Iterator<Item = (u64, Vec<BindingTarget>)> {
    let click_to_value = BinaryToScalarStates { off: 0.0, on: 1.0 };

    let mut entries = vec![];
    if let Some(source_click) = source.click {
        let mut targets = vec![];

        if let Some(destination_click) = destination.click {
            targets.push(passthrough(destination_click));
        }
        if source.touch.is_none()
            && let Some(destination_touch) = destination.touch
        {
            targets.push(passthrough(destination_touch));
        }

        if source.value.is_none()
            && let Some(destination_value) = destination.value
        {
            targets.push(binary_to_scalar(destination_value, click_to_value));
        }

        entries.push((source_click, targets));
    }
    if let Some(source_touch) = source.touch {
        let mut targets = vec![];
        if let Some(destination_touch) = destination.touch {
            targets.push(passthrough(destination_touch));
        }
        entries.push((source_touch, targets));
    }
    if let Some(source_value) = source.value {
        let mut targets = vec![];
        let mut remap_for_touch = false;
        let mut remap_for_force = false;

        if source.click.is_none()
            && let Some(destination_click) = destination.click
        {
            targets.push(hysteresis_threshold(
                destination_click,
                config.click_threshold,
            ));
        }
        if source.touch.is_none()
            && let Some(destination_touch) = destination.touch
        {
            targets.push(hysteresis_threshold(
                destination_touch,
                config.touch_threshold,
            ));
            remap_for_touch = true;
        }
        if source.force.is_none()
            && let Some(destination_force) = destination.force
        {
            targets.push(remap(
                destination_force,
                Range {
                    min: config.force_threshold,
                    max: 1.0,
                },
            ));
            remap_for_force = true;
        }

        if let Some(destination_value) = destination.value {
            if !remap_for_touch && !remap_for_force {
                targets.push(passthrough(destination_value));
            } else {
                let low = if remap_for_touch {
                    config.touch_threshold.value
                } else {
                    0.0
                };
                let high = if remap_for_force {
                    config.force_threshold
                } else {
                    1.0
                };
                targets.push(remap(
                    destination_value,
                    Range {
                        min: low,
                        max: high,
                    },
                ));
            }
        }

        entries.push((source_value, targets));
    }
    if let Some(source_force) = source.force {
        let mut targets = vec![];
        if let Some(destination_force) = destination.force {
            targets.push(passthrough(destination_force));
        }

        entries.push((source_force, targets));
    }

    entries.into_iter()
}

pub fn automatic_bindings(
    source_set: &HashSet<u64>,
    destination_set: &HashSet<u64>,
    config: &AutomaticButtonMappingConfig,
) -> HashMap<u64, Vec<BindingTarget>> {
    let s_set = source_set;
    let d_set = destination_set;

    let mut bindings = HashMap::new();

    // Menu buttons
    if s_set.contains(&*LEFT_MENU_CLICK_ID) {
        let click = click(*LEFT_MENU_CLICK_ID);
        if d_set.contains(&*LEFT_MENU_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(click, click, config));
        } else if d_set.contains(&*LEFT_SYSTEM_CLICK_ID) {
            let destination = ct(s_set, *LEFT_SYSTEM_CLICK_ID, *LEFT_SYSTEM_TOUCH_ID);
            // Drive the system touch too, but only if the emulated controller has it
            let destination = ButtonInputs {
                touch: destination.touch.filter(|touch| d_set.contains(touch)),
                ..destination
            };
            bindings.extend(map_button_pair_automatic(click, destination, config));
        }
    }
    if s_set.contains(&*LEFT_SYSTEM_CLICK_ID) {
        let source = ct(s_set, *LEFT_SYSTEM_CLICK_ID, *LEFT_SYSTEM_TOUCH_ID);
        // Bind touch only if both sides have it, sources without it keep a click-only binding
        let destination = ButtonInputs {
            touch: source.touch.filter(|touch| d_set.contains(touch)),
            ..source
        };
        if d_set.contains(&*LEFT_SYSTEM_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(source, destination, config));
        } else if d_set.contains(&*LEFT_MENU_CLICK_ID) {
            // A source without a menu button (Index) uses its system button as menu. Otherwise the
            // menu is already bound and the system button keeps its path, which the driver shares
            // with the menu button on SteamVR's side
            let destination = if s_set.contains(&*LEFT_MENU_CLICK_ID) {
                destination
            } else {
                click(*LEFT_MENU_CLICK_ID)
            };
            bindings.extend(map_button_pair_automatic(source, destination, config));
        }
    }
    if s_set.contains(&*RIGHT_MENU_CLICK_ID) {
        let click = click(*RIGHT_MENU_CLICK_ID);
        if d_set.contains(&*RIGHT_MENU_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(click, click, config));
        } else if d_set.contains(&*RIGHT_SYSTEM_CLICK_ID) {
            let destination = ct(s_set, *RIGHT_SYSTEM_CLICK_ID, *RIGHT_SYSTEM_TOUCH_ID);
            // Drive the system touch too, but only if the emulated controller has it
            let destination = ButtonInputs {
                touch: destination.touch.filter(|touch| d_set.contains(touch)),
                ..destination
            };
            bindings.extend(map_button_pair_automatic(click, destination, config));
        }
    }
    if s_set.contains(&*RIGHT_SYSTEM_CLICK_ID) {
        let source = ct(s_set, *RIGHT_SYSTEM_CLICK_ID, *RIGHT_SYSTEM_TOUCH_ID);
        // Bind touch only if both sides have it, sources without it keep a click-only binding
        let destination = ButtonInputs {
            touch: source.touch.filter(|touch| d_set.contains(touch)),
            ..source
        };
        if d_set.contains(&*RIGHT_SYSTEM_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(source, destination, config));
        } else if d_set.contains(&*RIGHT_MENU_CLICK_ID) {
            // A source without a menu button (Index) uses its system button as menu. Otherwise the
            // menu is already bound and the system button keeps its path, which the driver shares
            // with the menu button on SteamVR's side
            let destination = if s_set.contains(&*RIGHT_MENU_CLICK_ID) {
                destination
            } else {
                click(*RIGHT_MENU_CLICK_ID)
            };
            bindings.extend(map_button_pair_automatic(source, destination, config));
        }
    }

    // A/X buttons
    if s_set.contains(&*LEFT_X_CLICK_ID) {
        let source = ct(s_set, *LEFT_X_CLICK_ID, *LEFT_X_TOUCH_ID);
        if d_set.contains(&*LEFT_X_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_X_CLICK_ID, *LEFT_X_TOUCH_ID),
                config,
            ));
        } else if d_set.contains(&*LEFT_A_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_A_CLICK_ID, *LEFT_A_TOUCH_ID),
                config,
            ));
        } else if d_set.contains(&*LEFT_TRACKPAD_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_TRACKPAD_CLICK_ID, *LEFT_TRACKPAD_TOUCH_ID),
                config,
            ));
        }
    }
    if s_set.contains(&*RIGHT_A_CLICK_ID) {
        let source = ct(s_set, *RIGHT_A_CLICK_ID, *RIGHT_A_TOUCH_ID);
        if d_set.contains(&*RIGHT_A_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *RIGHT_A_CLICK_ID, *RIGHT_A_TOUCH_ID),
                config,
            ));
        } else if d_set.contains(&*RIGHT_TRACKPAD_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *RIGHT_TRACKPAD_CLICK_ID, *RIGHT_TRACKPAD_TOUCH_ID),
                config,
            ));
        }
    }

    // B/Y buttons
    if s_set.contains(&*LEFT_Y_CLICK_ID) {
        let source = ct(s_set, *LEFT_Y_CLICK_ID, *LEFT_Y_TOUCH_ID);
        if d_set.contains(&*LEFT_Y_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_Y_CLICK_ID, *LEFT_Y_TOUCH_ID),
                config,
            ));
        } else if d_set.contains(&*LEFT_B_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_B_CLICK_ID, *LEFT_B_TOUCH_ID),
                config,
            ));
        }
    }
    if s_set.contains(&*RIGHT_B_CLICK_ID) && d_set.contains(&*RIGHT_B_CLICK_ID) {
        bindings.extend(map_button_pair_automatic(
            ct(s_set, *RIGHT_B_CLICK_ID, *RIGHT_B_TOUCH_ID),
            ct(d_set, *RIGHT_B_CLICK_ID, *RIGHT_B_TOUCH_ID),
            config,
        ));
    }

    // Squeeze buttons
    if (s_set.contains(&*LEFT_SQUEEZE_CLICK_ID) || s_set.contains(&*LEFT_SQUEEZE_VALUE_ID))
        && (d_set.contains(&*LEFT_SQUEEZE_CLICK_ID) || d_set.contains(&*LEFT_SQUEEZE_VALUE_ID))
    {
        bindings.extend(map_button_pair_automatic(
            ctvf(
                s_set,
                *LEFT_SQUEEZE_CLICK_ID,
                *LEFT_SQUEEZE_TOUCH_ID,
                *LEFT_SQUEEZE_VALUE_ID,
                *LEFT_SQUEEZE_FORCE_ID,
            ),
            ctvf(
                d_set,
                *LEFT_SQUEEZE_CLICK_ID,
                *LEFT_SQUEEZE_TOUCH_ID,
                *LEFT_SQUEEZE_VALUE_ID,
                *LEFT_SQUEEZE_FORCE_ID,
            ),
            config,
        ));
    }
    if s_set.contains(&*LEFT_SQUEEZE_SENSOR_VALUE_ID)
        && d_set.contains(&*LEFT_SQUEEZE_SENSOR_VALUE_ID)
    {
        bindings.extend(map_button_pair_automatic(
            value(*LEFT_SQUEEZE_SENSOR_VALUE_ID),
            value(*LEFT_SQUEEZE_SENSOR_VALUE_ID),
            config,
        ));
    }
    if (s_set.contains(&*RIGHT_SQUEEZE_CLICK_ID) || s_set.contains(&*RIGHT_SQUEEZE_VALUE_ID))
        && (d_set.contains(&*RIGHT_SQUEEZE_CLICK_ID) || d_set.contains(&*RIGHT_SQUEEZE_VALUE_ID))
    {
        bindings.extend(map_button_pair_automatic(
            ctvf(
                s_set,
                *RIGHT_SQUEEZE_CLICK_ID,
                *RIGHT_SQUEEZE_TOUCH_ID,
                *RIGHT_SQUEEZE_VALUE_ID,
                *RIGHT_SQUEEZE_FORCE_ID,
            ),
            ctvf(
                d_set,
                *RIGHT_SQUEEZE_CLICK_ID,
                *RIGHT_SQUEEZE_TOUCH_ID,
                *RIGHT_SQUEEZE_VALUE_ID,
                *RIGHT_SQUEEZE_FORCE_ID,
            ),
            config,
        ));
    }
    if s_set.contains(&*RIGHT_SQUEEZE_SENSOR_VALUE_ID)
        && d_set.contains(&*RIGHT_SQUEEZE_SENSOR_VALUE_ID)
    {
        bindings.extend(map_button_pair_automatic(
            value(*RIGHT_SQUEEZE_SENSOR_VALUE_ID),
            value(*RIGHT_SQUEEZE_SENSOR_VALUE_ID),
            config,
        ));
    }
    // Trigger buttons
    if (s_set.contains(&*LEFT_TRIGGER_CLICK_ID) || s_set.contains(&*LEFT_TRIGGER_VALUE_ID))
        && (d_set.contains(&*LEFT_TRIGGER_CLICK_ID) || d_set.contains(&*LEFT_TRIGGER_VALUE_ID))
    {
        bindings.extend(map_button_pair_automatic(
            ctv(
                s_set,
                *LEFT_TRIGGER_CLICK_ID,
                *LEFT_TRIGGER_TOUCH_ID,
                *LEFT_TRIGGER_VALUE_ID,
            ),
            ctv(
                d_set,
                *LEFT_TRIGGER_CLICK_ID,
                *LEFT_TRIGGER_TOUCH_ID,
                *LEFT_TRIGGER_VALUE_ID,
            ),
            config,
        ));
    }
    if s_set.contains(&*LEFT_TRIGGER_SENSOR_VALUE_ID)
        && d_set.contains(&*LEFT_TRIGGER_SENSOR_VALUE_ID)
    {
        bindings.extend(map_button_pair_automatic(
            value(*LEFT_TRIGGER_SENSOR_VALUE_ID),
            value(*LEFT_TRIGGER_SENSOR_VALUE_ID),
            config,
        ));
    }
    if (s_set.contains(&*RIGHT_TRIGGER_CLICK_ID) || s_set.contains(&*RIGHT_TRIGGER_VALUE_ID))
        && (d_set.contains(&*RIGHT_TRIGGER_CLICK_ID) || d_set.contains(&*RIGHT_TRIGGER_VALUE_ID))
    {
        bindings.extend(map_button_pair_automatic(
            ctv(
                s_set,
                *RIGHT_TRIGGER_CLICK_ID,
                *RIGHT_TRIGGER_TOUCH_ID,
                *RIGHT_TRIGGER_VALUE_ID,
            ),
            ctv(
                d_set,
                *RIGHT_TRIGGER_CLICK_ID,
                *RIGHT_TRIGGER_TOUCH_ID,
                *RIGHT_TRIGGER_VALUE_ID,
            ),
            config,
        ));
    }
    if s_set.contains(&*RIGHT_TRIGGER_SENSOR_VALUE_ID)
        && d_set.contains(&*RIGHT_TRIGGER_SENSOR_VALUE_ID)
    {
        bindings.extend(map_button_pair_automatic(
            value(*RIGHT_TRIGGER_SENSOR_VALUE_ID),
            value(*RIGHT_TRIGGER_SENSOR_VALUE_ID),
            config,
        ));
    }
    // Thumbsticks
    if s_set.contains(&*LEFT_THUMBSTICK_X_ID) {
        let x = value(*LEFT_THUMBSTICK_X_ID);
        let y = value(*LEFT_THUMBSTICK_Y_ID);
        if d_set.contains(&*LEFT_THUMBSTICK_X_ID) {
            bindings.extend(map_button_pair_automatic(x, x, config));
            bindings.extend(map_button_pair_automatic(y, y, config));
        } else if d_set.contains(&*LEFT_TRACKPAD_X_ID) {
            bindings.extend(map_button_pair_automatic(
                x,
                value(*LEFT_TRACKPAD_X_ID),
                config,
            ));
            bindings.extend(map_button_pair_automatic(
                y,
                value(*LEFT_TRACKPAD_Y_ID),
                config,
            ));
        }
    }
    if s_set.contains(&*LEFT_THUMBSTICK_CLICK_ID) {
        let source = ct(s_set, *LEFT_THUMBSTICK_CLICK_ID, *LEFT_THUMBSTICK_TOUCH_ID);
        if d_set.contains(&*LEFT_THUMBSTICK_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_THUMBSTICK_CLICK_ID, *LEFT_THUMBSTICK_TOUCH_ID),
                config,
            ));
        } else if d_set.contains(&*LEFT_TRACKPAD_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *LEFT_TRACKPAD_CLICK_ID, *LEFT_TRACKPAD_TOUCH_ID),
                config,
            ));
        }
    }
    if s_set.contains(&*RIGHT_THUMBSTICK_X_ID) {
        let x = value(*RIGHT_THUMBSTICK_X_ID);
        let y = value(*RIGHT_THUMBSTICK_Y_ID);
        if d_set.contains(&*RIGHT_THUMBSTICK_X_ID) {
            bindings.extend(map_button_pair_automatic(x, x, config));
            bindings.extend(map_button_pair_automatic(y, y, config));
        } else if d_set.contains(&*RIGHT_TRACKPAD_X_ID) {
            bindings.extend(map_button_pair_automatic(
                x,
                value(*RIGHT_TRACKPAD_X_ID),
                config,
            ));
            bindings.extend(map_button_pair_automatic(
                y,
                value(*RIGHT_TRACKPAD_Y_ID),
                config,
            ));
        }
    }
    if s_set.contains(&*RIGHT_THUMBSTICK_CLICK_ID) {
        let source = ct(
            s_set,
            *RIGHT_THUMBSTICK_CLICK_ID,
            *RIGHT_THUMBSTICK_TOUCH_ID,
        );
        if d_set.contains(&*RIGHT_THUMBSTICK_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(
                    d_set,
                    *RIGHT_THUMBSTICK_CLICK_ID,
                    *RIGHT_THUMBSTICK_TOUCH_ID,
                ),
                config,
            ));
        } else if d_set.contains(&*RIGHT_TRACKPAD_CLICK_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                ct(d_set, *RIGHT_TRACKPAD_CLICK_ID, *RIGHT_TRACKPAD_TOUCH_ID),
                config,
            ));
        }
    }

    // Thumbrests
    if s_set.contains(&*LEFT_THUMBREST_TOUCH_ID) {
        let source = value(*LEFT_THUMBREST_TOUCH_ID);
        if d_set.contains(&*LEFT_THUMBREST_TOUCH_ID) {
            bindings.extend(map_button_pair_automatic(source, source, config));
        } else if d_set.contains(&*LEFT_TRACKPAD_TOUCH_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                value(*LEFT_TRACKPAD_TOUCH_ID),
                config,
            ));
        }
    }
    if s_set.contains(&*RIGHT_THUMBREST_TOUCH_ID) {
        let source = value(*RIGHT_THUMBREST_TOUCH_ID);
        if d_set.contains(&*RIGHT_THUMBREST_TOUCH_ID) {
            bindings.extend(map_button_pair_automatic(source, source, config));
        } else if d_set.contains(&*RIGHT_TRACKPAD_TOUCH_ID) {
            bindings.extend(map_button_pair_automatic(
                source,
                value(*RIGHT_TRACKPAD_TOUCH_ID),
                config,
            ));
        }
    }

    bindings
}

pub struct ButtonMappingManager {
    mappings: HashMap<u64, Vec<BindingTarget>>,
    binary_source_states: HashMap<u64, bool>,
    hysteresis_states: HashMap<u64, HashMap<u64, bool>>,
}

impl ButtonMappingManager {
    pub fn new_automatic(
        source: &HashSet<u64>,
        controllers_emulation_mode: &ControllersEmulationMode,
        button_mapping_config: &AutomaticButtonMappingConfig,
    ) -> Self {
        let button_set = registered_button_set(controllers_emulation_mode);
        Self {
            mappings: automatic_bindings(source, &button_set, button_mapping_config),
            binary_source_states: HashMap::new(),
            hysteresis_states: HashMap::new(),
        }
    }

    pub fn new_manual(mappings: &[(String, Vec<ButtonBindingTarget>)]) -> Self {
        let mappings = mappings
            .iter()
            .map(|(key, value)| {
                (
                    alvr_common::hash_string(key),
                    value
                        .iter()
                        .map(|b| BindingTarget {
                            destination: alvr_common::hash_string(&b.destination),
                            mapping_type: b.mapping_type.clone(),
                            binary_conditions: b
                                .binary_conditions
                                .iter()
                                .map(|c| alvr_common::hash_string(c))
                                .collect(),
                        })
                        .collect(),
                )
            })
            .collect();

        Self {
            mappings,
            binary_source_states: HashMap::new(),
            hysteresis_states: HashMap::new(),
        }
    }

    // Apply any button changes that are mapped to this specific button
    pub fn map_button(&mut self, source_button: &ButtonEntry) -> Vec<ButtonEntry> {
        if let ButtonValue::Binary(value) = source_button.value {
            let val_ref = self
                .binary_source_states
                .entry(source_button.path_id)
                .or_default();

            if value == *val_ref {
                return vec![];
            }

            // NB: Update value
            *val_ref = value;
        }

        let mut destination_buttons = vec![];

        if let Some(mappings) = self.mappings.get(&source_button.path_id) {
            'mapping: for mapping in mappings {
                let destination_value = match (&mapping.mapping_type, source_button.value) {
                    (ButtonMappingType::Passthrough, value) => value,
                    (
                        ButtonMappingType::HysteresisThreshold(threshold),
                        ButtonValue::Scalar(value),
                    ) => {
                        let state = self
                            .hysteresis_states
                            .entry(source_button.path_id)
                            .or_default()
                            .entry(mapping.destination)
                            .or_default();

                        if *state && value < threshold.value - threshold.deviation {
                            *state = false;
                        } else if !*state && value > threshold.value + threshold.deviation {
                            *state = true;
                        } else {
                            // No change needed
                            continue;
                        }

                        ButtonValue::Binary(*state)
                    }
                    (ButtonMappingType::BinaryToScalar(levels), ButtonValue::Binary(value)) => {
                        if value {
                            ButtonValue::Scalar(levels.on)
                        } else {
                            ButtonValue::Scalar(levels.off)
                        }
                    }
                    (ButtonMappingType::Remap(range), ButtonValue::Scalar(value)) => {
                        let value = (value - range.min) / (range.max - range.min);
                        ButtonValue::Scalar(value.clamp(0.0, 1.0))
                    }
                    _ => {
                        error!("Failed to map button!");
                        continue;
                    }
                };

                for source_id in &mapping.binary_conditions {
                    if !self
                        .binary_source_states
                        .get(source_id)
                        .copied()
                        .unwrap_or(false)
                    {
                        continue 'mapping;
                    }
                }

                destination_buttons.push(ButtonEntry {
                    path_id: mapping.destination,
                    value: destination_value,
                });
            }
        } else {
            let button_name = BUTTON_INFO
                .get(&source_button.path_id)
                .map_or("Unknown", |info| info.path);
            info!("Received button not mapped: {button_name}");
        }

        destination_buttons
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn test_config() -> AutomaticButtonMappingConfig {
        AutomaticButtonMappingConfig {
            click_threshold: HysteresisThreshold {
                value: 0.5,
                deviation: 0.05,
            },
            touch_threshold: HysteresisThreshold {
                value: 0.1,
                deviation: 0.05,
            },
            force_threshold: 0.8,
        }
    }

    fn short_path(id: u64) -> &'static str {
        BUTTON_INFO
            .get(&id)
            .unwrap()
            .path
            .trim_start_matches("/user/hand/")
    }

    // Human readable list of bindings, as "source -> destination (mapping)"
    fn binding_descriptions(
        source_profile: u64,
        emulation_mode: ControllersEmulationMode,
    ) -> BTreeSet<String> {
        let source_set = &CONTROLLER_PROFILE_INFO
            .get(&source_profile)
            .unwrap()
            .button_set;
        let destination_set = registered_button_set(&emulation_mode);

        let mut descriptions = BTreeSet::new();
        for (source, targets) in automatic_bindings(source_set, &destination_set, &test_config()) {
            for target in targets {
                let mapping = match target.mapping_type {
                    ButtonMappingType::Passthrough => "passthrough".to_owned(),
                    ButtonMappingType::HysteresisThreshold(threshold) => {
                        format!("threshold {}", threshold.value)
                    }
                    ButtonMappingType::BinaryToScalar(states) => {
                        format!("binary {}..{}", states.off, states.on)
                    }
                    ButtonMappingType::Remap(range) => {
                        format!("remap {}..{}", range.min, range.max)
                    }
                };
                descriptions.insert(format!(
                    "{} -> {} ({mapping})",
                    short_path(source),
                    short_path(target.destination)
                ));
            }
        }

        descriptions
    }

    fn assert_bindings(
        source_profile: u64,
        emulation_mode: ControllersEmulationMode,
        expected: &[&str],
    ) {
        let expected = expected
            .iter()
            .map(|description| description.to_string())
            .collect::<BTreeSet<_>>();

        assert_eq!(
            binding_descriptions(source_profile, emulation_mode),
            expected
        );
    }

    // Recorded before Index controller inputs were supported. Quest clients must keep these
    #[test]
    fn quest_to_index_bindings_are_unchanged() {
        assert_bindings(
            *QUEST_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::ValveIndex,
            &[
                "left/input/menu/click -> left/input/system/click (passthrough)",
                "left/input/squeeze/value -> left/input/squeeze/force (remap 0.8..1)",
                "left/input/squeeze/value -> left/input/squeeze/value (remap 0..0.8)",
                "left/input/thumbrest/touch -> left/input/trackpad/touch (passthrough)",
                "left/input/thumbstick/click -> left/input/thumbstick/click (passthrough)",
                "left/input/thumbstick/touch -> left/input/thumbstick/touch (passthrough)",
                "left/input/thumbstick/x -> left/input/thumbstick/x (passthrough)",
                "left/input/thumbstick/y -> left/input/thumbstick/y (passthrough)",
                "left/input/trigger/touch -> left/input/trigger/touch (passthrough)",
                "left/input/trigger/value -> left/input/trigger/click (threshold 0.5)",
                "left/input/trigger/value -> left/input/trigger/value (passthrough)",
                "left/input/x/click -> left/input/a/click (passthrough)",
                "left/input/x/touch -> left/input/a/touch (passthrough)",
                "left/input/y/click -> left/input/b/click (passthrough)",
                "left/input/y/touch -> left/input/b/touch (passthrough)",
                "right/input/a/click -> right/input/a/click (passthrough)",
                "right/input/a/touch -> right/input/a/touch (passthrough)",
                "right/input/b/click -> right/input/b/click (passthrough)",
                "right/input/b/touch -> right/input/b/touch (passthrough)",
                "right/input/squeeze/value -> right/input/squeeze/force (remap 0.8..1)",
                "right/input/squeeze/value -> right/input/squeeze/value (remap 0..0.8)",
                "right/input/system/click -> right/input/system/click (passthrough)",
                "right/input/thumbrest/touch -> right/input/trackpad/touch (passthrough)",
                "right/input/thumbstick/click -> right/input/thumbstick/click (passthrough)",
                "right/input/thumbstick/touch -> right/input/thumbstick/touch (passthrough)",
                "right/input/thumbstick/x -> right/input/thumbstick/x (passthrough)",
                "right/input/thumbstick/y -> right/input/thumbstick/y (passthrough)",
                "right/input/trigger/touch -> right/input/trigger/touch (passthrough)",
                "right/input/trigger/value -> right/input/trigger/click (threshold 0.5)",
                "right/input/trigger/value -> right/input/trigger/value (passthrough)",
            ],
        );
    }

    // Recorded before Index controller inputs were supported. Quest clients must keep these
    #[test]
    fn quest_to_quest_bindings_are_unchanged() {
        assert_bindings(
            *QUEST_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::Quest2Touch,
            &[
                "left/input/menu/click -> left/input/menu/click (passthrough)",
                "left/input/squeeze/value -> left/input/squeeze/value (passthrough)",
                "left/input/thumbrest/touch -> left/input/thumbrest/touch (passthrough)",
                "left/input/thumbstick/click -> left/input/thumbstick/click (passthrough)",
                "left/input/thumbstick/touch -> left/input/thumbstick/touch (passthrough)",
                "left/input/thumbstick/x -> left/input/thumbstick/x (passthrough)",
                "left/input/thumbstick/y -> left/input/thumbstick/y (passthrough)",
                "left/input/trigger/touch -> left/input/trigger/touch (passthrough)",
                "left/input/trigger/value -> left/input/trigger/value (passthrough)",
                "left/input/x/click -> left/input/x/click (passthrough)",
                "left/input/x/touch -> left/input/x/touch (passthrough)",
                "left/input/y/click -> left/input/y/click (passthrough)",
                "left/input/y/touch -> left/input/y/touch (passthrough)",
                "right/input/a/click -> right/input/a/click (passthrough)",
                "right/input/a/touch -> right/input/a/touch (passthrough)",
                "right/input/b/click -> right/input/b/click (passthrough)",
                "right/input/b/touch -> right/input/b/touch (passthrough)",
                "right/input/squeeze/value -> right/input/squeeze/value (passthrough)",
                "right/input/system/click -> right/input/system/click (passthrough)",
                "right/input/thumbrest/touch -> right/input/thumbrest/touch (passthrough)",
                "right/input/thumbstick/click -> right/input/thumbstick/click (passthrough)",
                "right/input/thumbstick/touch -> right/input/thumbstick/touch (passthrough)",
                "right/input/thumbstick/x -> right/input/thumbstick/x (passthrough)",
                "right/input/thumbstick/y -> right/input/thumbstick/y (passthrough)",
                "right/input/trigger/touch -> right/input/trigger/touch (passthrough)",
                "right/input/trigger/value -> right/input/trigger/value (passthrough)",
            ],
        );
    }

    // Recorded before Index controller inputs were supported. Quest clients must keep these
    #[test]
    fn quest_to_vive_bindings_are_unchanged() {
        assert_bindings(
            *QUEST_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::ViveWand,
            &[
                "left/input/menu/click -> left/input/menu/click (passthrough)",
                "left/input/squeeze/value -> left/input/squeeze/click (threshold 0.5)",
                "left/input/thumbrest/touch -> left/input/trackpad/touch (passthrough)",
                "left/input/thumbstick/click -> left/input/trackpad/click (passthrough)",
                "left/input/thumbstick/touch -> left/input/trackpad/touch (passthrough)",
                "left/input/thumbstick/x -> left/input/trackpad/x (passthrough)",
                "left/input/thumbstick/y -> left/input/trackpad/y (passthrough)",
                "left/input/trigger/value -> left/input/trigger/click (threshold 0.5)",
                "left/input/trigger/value -> left/input/trigger/value (passthrough)",
                "left/input/x/click -> left/input/trackpad/click (passthrough)",
                "left/input/x/touch -> left/input/trackpad/touch (passthrough)",
                "right/input/a/click -> right/input/trackpad/click (passthrough)",
                "right/input/a/touch -> right/input/trackpad/touch (passthrough)",
                "right/input/squeeze/value -> right/input/squeeze/click (threshold 0.5)",
                "right/input/system/click -> right/input/system/click (passthrough)",
                "right/input/thumbrest/touch -> right/input/trackpad/touch (passthrough)",
                "right/input/thumbstick/click -> right/input/trackpad/click (passthrough)",
                "right/input/thumbstick/touch -> right/input/trackpad/touch (passthrough)",
                "right/input/thumbstick/x -> right/input/trackpad/x (passthrough)",
                "right/input/thumbstick/y -> right/input/trackpad/y (passthrough)",
                "right/input/trigger/value -> right/input/trigger/click (threshold 0.5)",
                "right/input/trigger/value -> right/input/trigger/value (passthrough)",
            ],
        );
    }

    #[test]
    fn squeeze_force_is_forwarded_or_derived_from_value() {
        let descriptions = binding_descriptions(
            *INDEX_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::ValveIndex,
        );
        assert!(
            descriptions
                .contains("left/input/squeeze/force -> left/input/squeeze/force (passthrough)")
        );
        assert!(
            descriptions
                .contains("left/input/squeeze/value -> left/input/squeeze/value (passthrough)")
        );

        // Sources without force still emulate it from the end of the squeeze range
        let descriptions = binding_descriptions(
            *QUEST_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::ValveIndex,
        );
        assert!(
            descriptions
                .contains("left/input/squeeze/value -> left/input/squeeze/force (remap 0.8..1)")
        );
        assert!(
            descriptions
                .contains("left/input/squeeze/value -> left/input/squeeze/value (remap 0..0.8)")
        );
    }

    #[test]
    fn index_system_buttons_forward_touch_and_act_as_menu() {
        let descriptions = binding_descriptions(
            *INDEX_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::ValveIndex,
        );
        assert!(
            descriptions
                .contains("left/input/system/touch -> left/input/system/touch (passthrough)")
        );
        assert!(
            !descriptions
                .contains("left/input/system/click -> left/input/system/touch (passthrough)")
        );

        // Index has no menu button, the system button takes its place
        let descriptions = binding_descriptions(
            *INDEX_CONTROLLER_PROFILE_ID,
            ControllersEmulationMode::Quest2Touch,
        );
        assert!(
            descriptions.contains("left/input/system/click -> left/input/menu/click (passthrough)")
        );
    }

    #[test]
    fn system_touch_targets_registered_inputs_only() {
        for profile_info in CONTROLLER_PROFILE_INFO.values() {
            for emulation_mode in [
                ControllersEmulationMode::Quest2Touch,
                ControllersEmulationMode::Pico4,
                ControllersEmulationMode::PSVR2Sense,
                ControllersEmulationMode::ValveIndex,
                ControllersEmulationMode::ViveWand,
            ] {
                let destination_set = registered_button_set(&emulation_mode);
                let bindings =
                    automatic_bindings(&profile_info.button_set, &destination_set, &test_config());

                for target in bindings.values().flatten() {
                    if target.destination == *LEFT_SYSTEM_TOUCH_ID
                        || target.destination == *RIGHT_SYSTEM_TOUCH_ID
                    {
                        assert!(
                            destination_set.contains(&target.destination),
                            "{} binds unregistered {}",
                            profile_info.path,
                            BUTTON_INFO.get(&target.destination).unwrap().path
                        );
                    }
                }
            }
        }
    }
}
