use crate::hqm_game::{
    HQMGame, HQMObjectIndex, HQMPhysicsConfiguration, HQMPuck, HQMRink, HQMRinkLine, HQMRulesState,
    HQMTeam,
};
use crate::hqm_server::{HQMServer, HQMServerPlayer, HQMServerPlayerIndex, HQMServerPlayerList};

use crate::hqm_simulate::HQMSimulationEvent;
use nalgebra::{Point3, Rotation3, Vector3};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::f32::consts::{FRAC_PI_2, PI};

pub const ALLOWED_POSITIONS: [&str; 18] = [
    "C", "LW", "RW", "LD", "RD", "G", "LM", "RM", "LLM", "RRM", "LLD", "RRD", "CM", "CD", "LW2",
    "RW2", "LLW", "RRW",
];

#[derive(Debug, Clone)]
pub struct HQMFaceoffSpot {
    pub center_position: Point3<f32>,
    pub red_player_positions: HashMap<&'static str, (Point3<f32>, Rotation3<f32>)>,
    pub blue_player_positions: HashMap<&'static str, (Point3<f32>, Rotation3<f32>)>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum HQMRinkSide {
    Left,
    Right,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum HQMRinkFaceoffSpot {
    Center,
    DefensiveZone(HQMTeam, HQMRinkSide),
    Offside(HQMTeam, HQMRinkSide),
}

pub struct HQMMatchConfiguration {
    pub time_period: u32,
    pub time_warmup: u32,
    pub time_break: u32,
    pub time_intermission: u32,
    pub mercy: u32,
    pub first_to: u32,
    pub periods: u32,
    pub offside: HQMOffsideConfiguration,
    pub icing: HQMIcingConfiguration,
    pub offside_line: HQMOffsideLineConfiguration,
    pub twoline_pass: HQMTwoLinePassConfiguration,
    pub warmup_pucks: usize,
    pub physics_config: HQMPhysicsConfiguration,
    pub blue_line_location: f32,
    pub use_mph: bool,
    pub goal_replay: bool,
}

pub enum HQMMatchEvent {
    Goal {
        team: HQMTeam,
        goal: Option<HQMServerPlayerIndex>,
        assist: Option<HQMServerPlayerIndex>,
        speed: Option<f32>, // Raw meter/game tick (so meter per 1/100 of a second)
        speed_across_line: f32,
        time: u32,
        period: u32,
    },
}

pub struct HQMMatch {
    pub config: HQMMatchConfiguration,
    pub paused: bool,
    pub(crate) pause_timer: u32,
    is_pause_goal: bool,
    next_faceoff_spot: HQMRinkFaceoffSpot,
    icing_status: HQMIcingStatus,
    offside_status: HQMOffsideStatus,
    twoline_pass_status: HQMTwoLinePassStatus,
    pass: Option<HQMPass>,
    pub(crate) preferred_positions: HashMap<HQMServerPlayerIndex, &'static str>,

    pub started_as_goalie: Vec<HQMServerPlayerIndex>,
    faceoff_game_step: u32,
    step_where_period_ended: u32,
    too_late_printed_this_period: bool,
    start_next_replay: Option<(u32, u32, Option<HQMServerPlayerIndex>)>,
    puck_touches: HashMap<HQMObjectIndex, VecDeque<HQMPuckTouch>>,
}

impl HQMMatch {
    pub fn new(config: HQMMatchConfiguration) -> Self {
        Self {
            config,
            paused: false,
            pause_timer: 0,
            is_pause_goal: false,
            next_faceoff_spot: HQMRinkFaceoffSpot::Center,
            icing_status: HQMIcingStatus::No,
            offside_status: HQMOffsideStatus::Neutral,
            twoline_pass_status: HQMTwoLinePassStatus::No,
            pass: None,
            preferred_positions: HashMap::new(),
            started_as_goalie: vec![],
            faceoff_game_step: 0,
            too_late_printed_this_period: false,
            step_where_period_ended: 0,
            start_next_replay: None,
            puck_touches: Default::default(),
        }
    }

    pub fn clear_started_goalie(&mut self, player_index: HQMServerPlayerIndex) {
        if let Some(x) = self
            .started_as_goalie
            .iter()
            .position(|x| *x == player_index)
        {
            self.started_as_goalie.remove(x);
        }
    }

    fn do_faceoff(&mut self, server: &mut HQMServer) {
        let positions = get_faceoff_positions(&server.players, &self.preferred_positions);

        server.game.world.clear_pucks();
        self.puck_touches.clear();

        let next_faceoff_spot = get_faceoff_spot(&server.game.world.rink, self.next_faceoff_spot);

        let puck_pos = next_faceoff_spot.center_position + &(1.5f32 * Vector3::y());

        server
            .game
            .world
            .create_puck_object(puck_pos, Rotation3::identity());

        self.started_as_goalie.clear();
        for (player_index, (team, faceoff_position)) in positions {
            let (player_position, player_rotation) = match team {
                HQMTeam::Red => next_faceoff_spot.red_player_positions[faceoff_position].clone(),
                HQMTeam::Blue => next_faceoff_spot.blue_player_positions[faceoff_position].clone(),
            };
            server.spawn_skater(player_index, team, player_position, player_rotation);
            if faceoff_position == "G" {
                self.started_as_goalie.push(player_index);
            }
        }

        let rink = &server.game.world.rink;
        self.icing_status = HQMIcingStatus::No;
        self.offside_status = if rink
            .red_lines_and_net
            .offensive_line
            .point_past_middle_of_line(&puck_pos)
        {
            HQMOffsideStatus::InOffensiveZone(HQMTeam::Red)
        } else if rink
            .blue_lines_and_net
            .offensive_line
            .point_past_middle_of_line(&puck_pos)
        {
            HQMOffsideStatus::InOffensiveZone(HQMTeam::Blue)
        } else {
            HQMOffsideStatus::Neutral
        };
        self.twoline_pass_status = HQMTwoLinePassStatus::No;
        self.pass = None;

        self.faceoff_game_step = server.game_step;
    }

    pub(crate) fn update_game_over(&mut self, server: &mut HQMServer) {
        let time_gameover = self.config.time_intermission * 100;
        let time_break = self.config.time_break * 100;

        let red_score = server.game.red_score;
        let blue_score = server.game.blue_score;
        let old_game_over = server.game.game_over;
        server.game.game_over =
            if server.game.period > self.config.periods && red_score != blue_score {
                true
            } else if self.config.mercy > 0
                && (red_score.saturating_sub(blue_score) >= self.config.mercy
                    || blue_score.saturating_sub(red_score) >= self.config.mercy)
            {
                true
            } else if self.config.first_to > 0
                && (red_score >= self.config.first_to || blue_score >= self.config.first_to)
            {
                true
            } else {
                false
            };
        if server.game.game_over && !old_game_over {
            self.pause_timer = self.pause_timer.max(time_gameover);
        } else if !server.game.game_over && old_game_over {
            self.pause_timer = self.pause_timer.max(time_break);
        }
    }

    fn call_goal(
        &mut self,
        server: &mut HQMServer,
        team: HQMTeam,
        puck_index: HQMObjectIndex,
    ) -> HQMMatchEvent {
        let time_break = self.config.time_break * 100;

        match team {
            HQMTeam::Red => {
                server.game.red_score += 1;
            }
            HQMTeam::Blue => {
                server.game.blue_score += 1;
            }
        };

        self.next_faceoff_spot = HQMRinkFaceoffSpot::Center;

        let (
            goal_scorer_index,
            assist_index,
            puck_speed_across_line,
            puck_speed_from_stick,
            last_touch,
        ) = if let Some(this_puck) = server.game.world.objects.get_puck_mut(puck_index) {
            let mut goal_scorer_index = None;
            let mut assist_index = None;
            let mut goal_scorer_first_touch = 0;
            let mut puck_speed_from_stick = None;
            let mut last_touch = None;
            let puck_speed_across_line = this_puck.body.linear_velocity.norm();
            if let Some(touches) = self.puck_touches.get(&puck_index) {
                last_touch = touches.front().map(|x| x.player_index);

                for touch in touches.iter() {
                    if goal_scorer_index.is_none() {
                        if touch.team == team {
                            goal_scorer_index = Some(touch.player_index);
                            goal_scorer_first_touch = touch.first_time;
                            puck_speed_from_stick = Some(touch.puck_speed);
                        }
                    } else {
                        if touch.team == team {
                            if Some(touch.player_index) == goal_scorer_index {
                                goal_scorer_first_touch = touch.first_time;
                            } else {
                                // This is the first player on the scoring team that touched it apart from the goal scorer
                                // If more than 10 seconds passed between the goal scorer's first touch
                                // and this last touch, it doesn't count as an assist

                                let diff = touch.last_time.saturating_sub(goal_scorer_first_touch);

                                if diff <= 1000 {
                                    assist_index = Some(touch.player_index)
                                }
                                break;
                            }
                        }
                    }
                }
            }

            (
                goal_scorer_index,
                assist_index,
                puck_speed_across_line,
                puck_speed_from_stick,
                last_touch,
            )
        } else {
            (None, None, 0.0, None, None)
        };

        server
            .messages
            .add_goal_message(team, goal_scorer_index, assist_index);

        fn convert(puck_speed: f32, use_mph: bool) -> (f32, &'static str) {
            if use_mph {
                (puck_speed * 100f32 * 2.23693, "mph")
            } else {
                (puck_speed * 100f32 * 3.6, "km/h")
            }
        }

        let (puck_speed_across_line_converted, puck_speed_unit) =
            convert(puck_speed_across_line, self.config.use_mph);

        let str1 = format!(
            "Goal scored, {:.1} {} across line",
            puck_speed_across_line_converted, puck_speed_unit
        );

        let str2 = if let Some(puck_speed_from_stick) = puck_speed_from_stick {
            let (puck_speed_converted, puck_speed_unit) =
                convert(puck_speed_from_stick, self.config.use_mph);
            format!(
                ", {:.1} {} from stick",
                puck_speed_converted, puck_speed_unit
            )
        } else {
            "".to_owned()
        };
        let s = format!("{}{}", str1, str2);

        server.messages.add_server_chat_message(s);

        if server.game.time < 1000 {
            let time = server.game.time;
            let seconds = time / 100;
            let centi = time % 100;

            let s = format!("{}.{:02} seconds left", seconds, centi);
            server.messages.add_server_chat_message(s);
        }

        self.pause_timer = time_break;
        self.is_pause_goal = true;

        self.update_game_over(server);

        let gamestep = server.game_step;

        if self.config.goal_replay {
            let force_view = goal_scorer_index.or(last_touch);
            self.start_next_replay = Some((
                self.faceoff_game_step.max(gamestep - 600),
                gamestep + 200,
                force_view,
            ));

            self.pause_timer = self.pause_timer.saturating_sub(800).max(400);
        }
        HQMMatchEvent::Goal {
            team,
            time: server.game.time,
            period: server.game.period,
            goal: goal_scorer_index,
            assist: assist_index,
            speed: puck_speed_from_stick,
            speed_across_line: puck_speed_across_line,
        }
    }

    fn handle_events_end_of_period(
        &mut self,
        server: &mut HQMServer,
        events: &[HQMSimulationEvent],
    ) {
        for event in events {
            if let HQMSimulationEvent::PuckEnteredNet { .. } = event {
                let time = server
                    .game_step
                    .saturating_sub(self.step_where_period_ended);
                if time <= 300 && !self.too_late_printed_this_period {
                    let seconds = time / 100;
                    let centi = time % 100;
                    self.too_late_printed_this_period = true;
                    let s = format!("{}.{:02} seconds too late!", seconds, centi);

                    server.messages.add_server_chat_message(s);
                }
            }
        }
    }

    fn handle_puck_touch(
        &mut self,
        server: &mut HQMServer,
        player: HQMObjectIndex,
        puck_index: HQMObjectIndex,
    ) {
        if let Some((player_index, touching_team, _)) = server.players.get_from_object_index(player)
        {
            if let Some(puck) = server.game.world.objects.get_puck_mut(puck_index) {
                add_touch(
                    puck,
                    self.puck_touches.entry(puck_index),
                    player_index,
                    player,
                    touching_team,
                    server.game.time,
                );
                let side = if puck.body.pos.x <= &server.game.world.rink.width / 2.0 {
                    HQMRinkSide::Left
                } else {
                    HQMRinkSide::Right
                };
                self.pass = Some(HQMPass {
                    team: touching_team,
                    side,
                    from: None,
                    player: player_index,
                });

                let other_team = touching_team.get_other_team();

                if let HQMOffsideStatus::Warning(team, side, position, i) = self.offside_status {
                    if team == touching_team {
                        let self_touch = player_index == i;

                        self.call_offside(server, touching_team, side, position, self_touch);
                        return;
                    }
                }
                if let HQMTwoLinePassStatus::Warning(team, side, position, ref i) =
                    self.twoline_pass_status
                {
                    if team == touching_team && i.contains(&player_index) {
                        self.call_twoline_pass(server, touching_team, side, position);
                        return;
                    } else {
                        self.twoline_pass_status = HQMTwoLinePassStatus::No;
                        server
                            .messages
                            .add_server_chat_message_str("Two-line pass waved off");
                    }
                }
                if let HQMIcingStatus::Warning(team, side) = self.icing_status {
                    if touching_team != team && !self.started_as_goalie.contains(&player_index) {
                        self.call_icing(server, other_team, side);
                    } else {
                        self.icing_status = HQMIcingStatus::No;
                        server
                            .messages
                            .add_server_chat_message_str("Icing waved off");
                    }
                }
            }
        }
    }

    fn handle_puck_entered_net(
        &mut self,
        server: &mut HQMServer,
        events: &mut Vec<HQMMatchEvent>,
        team: HQMTeam,
        puck: HQMObjectIndex,
    ) {
        match self.offside_status {
            HQMOffsideStatus::Warning(offside_team, side, position, _) if offside_team == team => {
                self.call_offside(server, team, side, position, false);
            }
            HQMOffsideStatus::Offside(_) => {}
            _ => {
                events.push(self.call_goal(server, team, puck));
            }
        }
    }

    fn handle_puck_passed_goal_line(&mut self, server: &mut HQMServer, team: HQMTeam) {
        if let Some(HQMPass {
            team: icing_team,
            side,
            from: Some(transition),
            ..
        }) = self.pass
        {
            if team == icing_team && transition <= HQMPassPosition::ReachedCenter {
                match self.config.icing {
                    HQMIcingConfiguration::Touch => {
                        self.icing_status = HQMIcingStatus::Warning(team, side);
                        server.messages.add_server_chat_message_str("Icing warning");
                    }
                    HQMIcingConfiguration::NoTouch => {
                        self.call_icing(server, team, side);
                    }
                    HQMIcingConfiguration::Off => {}
                }
            }
        }
    }

    fn puck_into_offside_zone(&mut self, server: &mut HQMServer, team: HQMTeam) {
        if self.offside_status == HQMOffsideStatus::InOffensiveZone(team) {
            return;
        }
        if let Some(HQMPass {
            team: pass_team,
            side,
            from: transition,
            player,
        }) = self.pass
        {
            if team == pass_team && has_players_in_offensive_zone(&server, team, Some(player)) {
                match self.config.offside {
                    HQMOffsideConfiguration::Delayed => {
                        self.offside_status =
                            HQMOffsideStatus::Warning(team, side, transition, player);
                        server
                            .messages
                            .add_server_chat_message_str("Offside warning");
                    }
                    HQMOffsideConfiguration::Immediate => {
                        self.call_offside(server, team, side, transition, false);
                    }
                    HQMOffsideConfiguration::Off => {
                        self.offside_status = HQMOffsideStatus::InOffensiveZone(team);
                    }
                }
            } else {
                self.offside_status = HQMOffsideStatus::InOffensiveZone(team);
            }
        } else {
            self.offside_status = HQMOffsideStatus::InOffensiveZone(team);
        }
    }

    fn handle_puck_entered_offensive_half(&mut self, server: &mut HQMServer, team: HQMTeam) {
        if !matches!(&self.offside_status, HQMOffsideStatus::Offside(_))
            && self.config.offside_line == HQMOffsideLineConfiguration::Center
        {
            self.puck_into_offside_zone(server, team);
        }
        if let HQMOffsideStatus::Warning(warning_team, _, _, _) = self.offside_status {
            if warning_team != team {
                server
                    .messages
                    .add_server_chat_message_str("Offside waved off");
            }
        }
        if let Some(HQMPass {
            team: pass_team,
            side,
            from: Some(from),
            player: pass_player,
        }) = self.pass
        {
            if self.twoline_pass_status == HQMTwoLinePassStatus::No && pass_team == team {
                let is_regular_twoline_pass_active = self.config.twoline_pass
                    == HQMTwoLinePassConfiguration::Double
                    || self.config.twoline_pass == HQMTwoLinePassConfiguration::On;
                if from <= HQMPassPosition::ReachedOwnBlue && is_regular_twoline_pass_active {
                    self.check_twoline_pass(server, team, side, from, pass_player, false);
                }
            }
        }
    }

    fn handle_puck_entered_offensive_zone(&mut self, server: &mut HQMServer, team: HQMTeam) {
        if !matches!(&self.offside_status, HQMOffsideStatus::Offside(_))
            && self.config.offside_line == HQMOffsideLineConfiguration::OffensiveBlue
        {
            self.puck_into_offside_zone(server, team);
        }
        if let Some(HQMPass {
            team: pass_team,
            side,
            from: Some(from),
            player: pass_player,
        }) = self.pass
        {
            if self.twoline_pass_status == HQMTwoLinePassStatus::No && pass_team == team {
                let is_forward_twoline_pass_active = self.config.twoline_pass
                    == HQMTwoLinePassConfiguration::Double
                    || self.config.twoline_pass == HQMTwoLinePassConfiguration::Forward;
                let is_threeline_pass_active =
                    self.config.twoline_pass == HQMTwoLinePassConfiguration::ThreeLine;
                if (from <= HQMPassPosition::ReachedCenter && is_forward_twoline_pass_active)
                    || from <= HQMPassPosition::ReachedOwnBlue && is_threeline_pass_active
                {
                    self.check_twoline_pass(server, team, side, from, pass_player, true);
                }
            }
        }
    }

    fn check_twoline_pass(
        &mut self,
        server: &mut HQMServer,
        team: HQMTeam,
        side: HQMRinkSide,
        from: HQMPassPosition,
        pass_player: HQMServerPlayerIndex,
        is_offensive_line: bool,
    ) {
        let team_line = match team {
            HQMTeam::Red => &server.game.world.rink.red_lines_and_net,
            HQMTeam::Blue => &server.game.world.rink.blue_lines_and_net,
        };
        let line = if is_offensive_line {
            &team_line.offensive_line
        } else {
            &team_line.mid_line
        };
        let mut players_past_line = vec![];
        for (player_index, player) in server.players.iter() {
            if player_index == pass_player {
                continue;
            }
            if is_past_line(server, player, team, line) {
                players_past_line.push(player_index);
            }
        }
        if !players_past_line.is_empty() {
            self.twoline_pass_status =
                HQMTwoLinePassStatus::Warning(team, side, from, players_past_line);
            server
                .messages
                .add_server_chat_message_str("Two-line pass warning");
        }
    }

    fn handle_puck_passed_defensive_line(&mut self, server: &mut HQMServer, team: HQMTeam) {
        if !matches!(&self.offside_status, HQMOffsideStatus::Offside(_))
            && self.config.offside_line == HQMOffsideLineConfiguration::OffensiveBlue
        {
            if let HQMOffsideStatus::Warning(t, _, _, _) = self.offside_status {
                if team.get_other_team() == t {
                    server
                        .messages
                        .add_server_chat_message_str("Offside waved off");
                }
            }
            self.offside_status = HQMOffsideStatus::Neutral;
        }
    }

    fn update_pass(&mut self, team: HQMTeam, p: HQMPassPosition) {
        if let Some(pass) = &mut self.pass {
            if pass.team == team && pass.from.is_none() {
                pass.from = Some(p);
            }
        }
    }

    fn check_wave_off_twoline(&mut self, server: &mut HQMServer, team: HQMTeam) {
        if let HQMTwoLinePassStatus::Warning(warning_team, _, _, _) = self.twoline_pass_status {
            if team != warning_team {
                self.twoline_pass_status = HQMTwoLinePassStatus::No;
                server
                    .messages
                    .add_server_chat_message_str("Two-line pass waved off");
            }
        }
    }

    fn handle_events(
        &mut self,
        server: &mut HQMServer,
        events: &[HQMSimulationEvent],
        match_events: &mut Vec<HQMMatchEvent>,
    ) {
        for event in events {
            match *event {
                HQMSimulationEvent::PuckEnteredNet { team, puck } => {
                    self.handle_puck_entered_net(server, match_events, team, puck);
                }
                HQMSimulationEvent::PuckTouch { player, puck, .. } => {
                    self.handle_puck_touch(server, player, puck);
                }
                HQMSimulationEvent::PuckReachedDefensiveLine { team, puck: _ } => {
                    self.check_wave_off_twoline(server, team);
                    self.update_pass(team, HQMPassPosition::ReachedOwnBlue);
                }
                HQMSimulationEvent::PuckPassedDefensiveLine { team, puck: _ } => {
                    self.update_pass(team, HQMPassPosition::PassedOwnBlue);
                    self.handle_puck_passed_defensive_line(server, team);
                }
                HQMSimulationEvent::PuckReachedCenterLine { team, puck: _ } => {
                    self.check_wave_off_twoline(server, team);
                    self.update_pass(team, HQMPassPosition::ReachedCenter);
                }
                HQMSimulationEvent::PuckPassedCenterLine { team, puck: _ } => {
                    self.update_pass(team, HQMPassPosition::PassedCenter);
                    self.handle_puck_entered_offensive_half(server, team);
                }
                HQMSimulationEvent::PuckReachedOffensiveZone { team, puck: _ } => {
                    self.update_pass(team, HQMPassPosition::ReachedOffensive);
                }
                HQMSimulationEvent::PuckEnteredOffensiveZone { team, puck: _ } => {
                    self.update_pass(team, HQMPassPosition::PassedOffensive);
                    self.handle_puck_entered_offensive_zone(server, team);
                }
                HQMSimulationEvent::PuckPassedGoalLine { team, puck: _ } => {
                    self.handle_puck_passed_goal_line(server, team);
                }
                _ => {}
            }

            if self.pause_timer > 0
                || server.game.time == 0
                || server.game.game_over
                || server.game.period == 0
            {
                return;
            }
        }
    }

    fn call_offside(
        &mut self,
        server: &mut HQMServer,
        team: HQMTeam,
        side: HQMRinkSide,
        position: Option<HQMPassPosition>,
        self_touch: bool,
    ) {
        let time_break = self.config.time_break * 100;

        let faceoff_spot = if self_touch {
            match self.config.offside_line {
                HQMOffsideLineConfiguration::OffensiveBlue => {
                    HQMRinkFaceoffSpot::Offside(team.get_other_team(), side)
                }
                HQMOffsideLineConfiguration::Center => HQMRinkFaceoffSpot::Center,
            }
        } else {
            match position {
                Some(p) if p <= HQMPassPosition::ReachedOwnBlue => {
                    HQMRinkFaceoffSpot::DefensiveZone(team, side)
                }
                Some(p) if p <= HQMPassPosition::ReachedCenter => {
                    HQMRinkFaceoffSpot::Offside(team, side)
                }
                _ => HQMRinkFaceoffSpot::Center,
            }
        };

        self.next_faceoff_spot = faceoff_spot;
        self.pause_timer = time_break;
        self.offside_status = HQMOffsideStatus::Offside(team);
        server.messages.add_server_chat_message_str("Offside");
    }

    fn call_twoline_pass(
        &mut self,
        server: &mut HQMServer,
        team: HQMTeam,
        side: HQMRinkSide,
        position: HQMPassPosition,
    ) {
        let time_break = self.config.time_break * 100;

        let faceoff_spot = if position <= HQMPassPosition::ReachedOwnBlue {
            HQMRinkFaceoffSpot::DefensiveZone(team, side)
        } else if position <= HQMPassPosition::ReachedCenter {
            HQMRinkFaceoffSpot::Offside(team, side)
        } else {
            HQMRinkFaceoffSpot::Center
        };

        self.next_faceoff_spot = faceoff_spot;
        self.pause_timer = time_break;
        self.twoline_pass_status = HQMTwoLinePassStatus::Offside(team);
        server.messages.add_server_chat_message_str("Two-line pass");
    }

    fn call_icing(&mut self, server: &mut HQMServer, team: HQMTeam, side: HQMRinkSide) {
        let time_break = self.config.time_break * 100;

        self.next_faceoff_spot = HQMRinkFaceoffSpot::DefensiveZone(team, side);
        self.pause_timer = time_break;
        self.icing_status = HQMIcingStatus::Icing(team);
        server.messages.add_server_chat_message_str("Icing");
    }

    pub fn after_tick(
        &mut self,
        server: &mut HQMServer,
        events: &[HQMSimulationEvent],
    ) -> Vec<HQMMatchEvent> {
        let mut match_events = vec![];
        if server.game.time == 0 && server.game.period > 1 {
            self.handle_events_end_of_period(server, events);
        } else if self.pause_timer > 0
            || server.game.time == 0
            || server.game.game_over
            || server.game.period == 0
            || self.paused
        {
            // Nothing
        } else {
            self.handle_events(server, events, &mut match_events);

            if let HQMOffsideStatus::Warning(team, _, _, _) = self.offside_status {
                if !has_players_in_offensive_zone(server, team, None) {
                    self.offside_status = HQMOffsideStatus::InOffensiveZone(team);
                    server
                        .messages
                        .add_server_chat_message_str("Offside waved off");
                }
            }

            let rules_state = if matches!(self.offside_status, HQMOffsideStatus::Offside(_))
                || matches!(self.twoline_pass_status, HQMTwoLinePassStatus::Offside(_))
            {
                HQMRulesState::Offside
            } else if matches!(self.icing_status, HQMIcingStatus::Icing(_)) {
                HQMRulesState::Icing
            } else {
                let icing_warning = matches!(self.icing_status, HQMIcingStatus::Warning(_, _));
                let offside_warning =
                    matches!(self.offside_status, HQMOffsideStatus::Warning(_, _, _, _))
                        || matches!(
                            self.twoline_pass_status,
                            HQMTwoLinePassStatus::Warning(_, _, _, _)
                        );
                HQMRulesState::Regular {
                    offside_warning,
                    icing_warning,
                }
            };

            server.game.rules_state = rules_state;
        }

        self.update_clock(server);

        if let Some((start_replay, end_replay, force_view)) = self.start_next_replay {
            if end_replay <= server.game_step {
                server.add_replay_to_queue(start_replay, end_replay, force_view);
                server.messages.add_server_chat_message_str("Goal replay");
                self.start_next_replay = None;
            }
        }
        match_events
    }

    fn update_clock(&mut self, server: &mut HQMServer) {
        let period_length = self.config.time_period * 100;
        let intermission_time = self.config.time_intermission * 100;

        if !self.paused {
            if self.pause_timer > 0 {
                self.pause_timer -= 1;
                if self.pause_timer == 0 {
                    self.is_pause_goal = false;
                    if server.game.game_over {
                        server.new_game(self.create_game());
                    } else {
                        if server.game.time == 0 {
                            server.game.time = period_length;
                        }

                        self.do_faceoff(server);
                    }
                }
            } else {
                server.game.time = server.game.time.saturating_sub(1);
                if server.game.time == 0 {
                    server.game.period += 1;
                    self.pause_timer = intermission_time;
                    self.is_pause_goal = false;
                    self.step_where_period_ended = server.game_step;
                    self.too_late_printed_this_period = false;
                    self.next_faceoff_spot = HQMRinkFaceoffSpot::Center;
                    self.update_game_over(server);
                }
            }
        }
        server.game.goal_message_timer = if self.is_pause_goal {
            self.pause_timer
        } else {
            0
        };
    }

    pub fn cleanup_player(&mut self, player_index: HQMServerPlayerIndex) {
        if let Some(x) = self
            .started_as_goalie
            .iter()
            .position(|x| *x == player_index)
        {
            self.started_as_goalie.remove(x);
        }
        self.preferred_positions.remove(&player_index);
    }

    pub fn create_game(&mut self) -> HQMGame {
        self.paused = false;
        self.pause_timer = 0;
        self.next_faceoff_spot = HQMRinkFaceoffSpot::Center;
        self.icing_status = HQMIcingStatus::No;
        self.offside_status = HQMOffsideStatus::Neutral;
        self.twoline_pass_status = HQMTwoLinePassStatus::No;
        self.start_next_replay = None;

        let warmup_pucks = self.config.warmup_pucks;

        let mut game = HQMGame::new(
            warmup_pucks,
            self.config.physics_config.clone(),
            self.config.blue_line_location,
        );
        let puck_line_start = game.world.rink.width / 2.0 - 0.4 * ((warmup_pucks - 1) as f32);

        for i in 0..warmup_pucks {
            let pos = Point3::new(
                puck_line_start + 0.8 * (i as f32),
                1.5,
                game.world.rink.length / 2.0,
            );
            let rot = Rotation3::identity();
            game.world.create_puck_object(pos, rot);
        }
        game.time = self.config.time_warmup * 100;
        game
    }
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMIcingConfiguration {
    Off,
    Touch,
    NoTouch,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMOffsideConfiguration {
    Off,
    Delayed,
    Immediate,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMTwoLinePassConfiguration {
    Off,
    On,
    Forward,
    Double,
    ThreeLine,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMOffsideLineConfiguration {
    OffensiveBlue,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HQMPassPosition {
    None,
    ReachedOwnBlue,
    PassedOwnBlue,
    ReachedCenter,
    PassedCenter,
    ReachedOffensive,
    PassedOffensive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HQMPass {
    pub team: HQMTeam,
    pub side: HQMRinkSide,
    pub from: Option<HQMPassPosition>,
    pub player: HQMServerPlayerIndex,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum HQMIcingStatus {
    No,                            // No icing
    Warning(HQMTeam, HQMRinkSide), // Puck has reached the goal line, delayed icing
    Icing(HQMTeam),                // Icing has been called
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum HQMOffsideStatus {
    Neutral,                  // No offside
    InOffensiveZone(HQMTeam), // No offside, puck in offensive zone
    Warning(
        HQMTeam,
        HQMRinkSide,
        Option<HQMPassPosition>,
        HQMServerPlayerIndex,
    ), // Warning, puck entered offensive zone in an offside situation but not touched yet
    Offside(HQMTeam),         // Offside has been called
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum HQMTwoLinePassStatus {
    No, // No offside
    Warning(
        HQMTeam,
        HQMRinkSide,
        HQMPassPosition,
        Vec<HQMServerPlayerIndex>,
    ), // Warning, puck entered offensive zone in an offside situation but not touched yet
    Offside(HQMTeam), // Offside has been called
}

#[derive(Debug, Clone)]
pub struct HQMPuckTouch {
    pub player_index: HQMServerPlayerIndex,
    pub skater_index: HQMObjectIndex,
    pub team: HQMTeam,
    pub puck_pos: Point3<f32>,
    pub puck_speed: f32,
    pub first_time: u32,
    pub last_time: u32,
}

pub fn add_touch(
    puck: &HQMPuck,
    entry: Entry<HQMObjectIndex, VecDeque<HQMPuckTouch>>,
    player_index: HQMServerPlayerIndex,
    skater_index: HQMObjectIndex,
    team: HQMTeam,
    time: u32,
) {
    let puck_pos = puck.body.pos.clone();
    let puck_speed = puck.body.linear_velocity.norm();

    let touches = entry.or_insert_with(|| VecDeque::new());
    let most_recent_touch = touches.front_mut();

    match most_recent_touch {
        Some(most_recent_touch)
            if most_recent_touch.player_index == player_index && most_recent_touch.team == team =>
        {
            most_recent_touch.puck_pos = puck_pos;
            most_recent_touch.last_time = time;
            most_recent_touch.puck_speed = puck_speed;
        }
        _ => {
            touches.truncate(15);
            touches.push_front(HQMPuckTouch {
                player_index,
                skater_index,
                team,
                puck_pos,
                puck_speed,
                first_time: time,
                last_time: time,
            });
        }
    }
}

pub fn get_faceoff_positions(
    players: &HQMServerPlayerList,
    preferred_positions: &HashMap<HQMServerPlayerIndex, &'static str>,
) -> HashMap<HQMServerPlayerIndex, (HQMTeam, &'static str)> {
    let mut res = HashMap::new();

    let mut red_players = smallvec::SmallVec::<[_; 32]>::new();
    let mut blue_players = smallvec::SmallVec::<[_; 32]>::new();
    for (player_index, player) in players.iter() {
        let team = player.object.map(|x| x.1);

        let preferred_position = preferred_positions.get(&player_index).map(|x| *x);

        if team == Some(HQMTeam::Red) {
            red_players.push((player_index, preferred_position));
        } else if team == Some(HQMTeam::Blue) {
            blue_players.push((player_index, preferred_position));
        }
    }

    setup_position(&mut res, &red_players, HQMTeam::Red);
    setup_position(&mut res, &blue_players, HQMTeam::Blue);

    res
}

pub fn is_past_line(
    server: &HQMServer,
    player: &HQMServerPlayer,
    team: HQMTeam,
    line: &HQMRinkLine,
) -> bool {
    if let Some((object_index, skater_team)) = player.object {
        if skater_team == team {
            if let Some(skater) = server.game.world.objects.get_skater(object_index) {
                let feet_pos =
                    &skater.body.pos - (&skater.body.rot * Vector3::y().scale(skater.height));
                let dot = (&feet_pos - &line.point).dot(&line.normal);
                let leading_edge = -(line.width / 2.0);
                if dot < leading_edge {
                    // Player is past line
                    return true;
                }
            }
        }
    }
    false
}

pub fn has_players_in_offensive_zone(
    server: &HQMServer,
    team: HQMTeam,
    ignore_player: Option<HQMServerPlayerIndex>,
) -> bool {
    let line = match team {
        HQMTeam::Red => &server.game.world.rink.red_lines_and_net.offensive_line,
        HQMTeam::Blue => &server.game.world.rink.blue_lines_and_net.offensive_line,
    };

    for (player_index, player) in server.players.iter() {
        if Some(player_index) == ignore_player {
            continue;
        }
        if is_past_line(server, player, team, line) {
            return true;
        }
    }

    false
}

fn setup_position(
    positions: &mut HashMap<HQMServerPlayerIndex, (HQMTeam, &'static str)>,
    players: &[(HQMServerPlayerIndex, Option<&'static str>)],
    team: HQMTeam,
) {
    let mut available_positions = Vec::from(ALLOWED_POSITIONS);

    // First, we try to give each player its preferred position
    for (player_index, player_position) in players.iter() {
        if let Some(player_position) = player_position {
            if let Some(x) = available_positions
                .iter()
                .position(|x| x == player_position)
            {
                let s = available_positions.remove(x);
                positions.insert(*player_index, (team, s));
            }
        }
    }

    // Some players did not get their preferred positions because they didn't have one,
    // or because it was already taken
    for (player_index, player_position) in players.iter() {
        if !positions.contains_key(player_index) {
            let s = if let Some(x) = available_positions.iter().position(|x| *x == "C") {
                // Someone needs to be C
                let x = available_positions.remove(x);
                (team, x)
            } else if !available_positions.is_empty() {
                // Give out the remaining positions
                let x = available_positions.remove(0);
                (team, x)
            } else {
                // Oh no, we're out of legal starting positions
                if let Some(player_position) = player_position {
                    (team, *player_position)
                } else {
                    (team, "C")
                }
            };
            positions.insert(*player_index, s);
        }
    }

    if let Some(x) = available_positions.iter().position(|x| *x == "C") {
        let mut change_index = None;
        for (player_index, _) in players.iter() {
            if change_index.is_none() {
                change_index = Some(player_index);
            }

            if let Some((_, pos)) = positions.get(player_index) {
                if *pos != "G" {
                    change_index = Some(player_index);
                    break;
                }
            }
        }

        if let Some(change_index) = change_index {
            let c = available_positions.remove(x);
            positions.insert(*change_index, (team, c));
        }
    }
}

fn get_faceoff_spot(rink: &HQMRink, spot: HQMRinkFaceoffSpot) -> HQMFaceoffSpot {
    let length = rink.length;
    let width = rink.width;

    let red_rot = Rotation3::identity();
    let blue_rot = Rotation3::from_euler_angles(0.0, PI, 0.0);
    let red_goalie_pos = Point3::new(width / 2.0, 1.5, length - 5.0);
    let blue_goalie_pos = Point3::new(width / 2.0, 1.5, 5.0);

    let goal_line_distance = 4.0; // IIHF rule 17iv

    let blue_line_distance_neutral_zone_edge = rink.blue_line_distance;
    // IIHF specifies distance between end boards and edge closest to the neutral zone, but my code specifies middle of line
    let distance_neutral_faceoff_spot = blue_line_distance_neutral_zone_edge + 1.5; // IIHF rule 18iv and 18vii
    let distance_zone_faceoff_spot = goal_line_distance + 6.0; // IIHF rule 18vi and 18vii

    let center_x = width / 2.0;
    let left_faceoff_x = center_x - 7.0; // IIHF rule 18vi and 18iv
    let right_faceoff_x = center_x + 7.0; // IIHF rule 18vi and 18iv

    let red_zone_faceoff_z = length - distance_zone_faceoff_spot;
    let red_neutral_faceoff_z = length - distance_neutral_faceoff_spot;
    let center_z = length / 2.0;
    let blue_neutral_faceoff_z = distance_neutral_faceoff_spot;
    let blue_zone_faceoff_z = distance_zone_faceoff_spot;

    let create_faceoff_spot = |center_position: Point3<f32>| {
        let red_defensive_zone = center_position.z > length - 11.0;
        let blue_defensive_zone = center_position.z < 11.0;
        let (red_left, red_right) = if center_position.x < 9.0 {
            (true, false)
        } else if center_position.x > width - 9.0 {
            (false, true)
        } else {
            (false, false)
        };
        let blue_left = red_right;
        let blue_right = red_left;

        fn get_positions(
            center_position: &Point3<f32>,
            rot: &Rotation3<f32>,
            goalie_pos: &Point3<f32>,
            is_defensive_zone: bool,
            is_close_to_left: bool,
            is_close_to_right: bool,
        ) -> HashMap<&'static str, (Point3<f32>, Rotation3<f32>)> {
            let mut player_positions = HashMap::new();

            let winger_z = 4.0;
            let m_z = 7.25;
            let d_z = if is_defensive_zone { 8.25 } else { 10.0 };
            let (far_left_winger_x, far_left_winger_z) = if is_close_to_left {
                (-6.5, 3.0)
            } else {
                (-10.0, winger_z)
            };
            let (far_right_winger_x, far_right_winger_z) = if is_close_to_right {
                (6.5, 3.0)
            } else {
                (10.0, winger_z)
            };

            let offsets = vec![
                ("C", Vector3::new(0.0, 1.5, 2.75)),
                ("LM", Vector3::new(-2.0, 1.5, m_z)),
                ("RM", Vector3::new(2.0, 1.5, m_z)),
                ("LW", Vector3::new(-5.0, 1.5, winger_z)),
                ("RW", Vector3::new(5.0, 1.5, winger_z)),
                ("LD", Vector3::new(-2.0, 1.5, d_z)),
                ("RD", Vector3::new(2.0, 1.5, d_z)),
                (
                    "LLM",
                    Vector3::new(
                        if is_close_to_left && is_defensive_zone {
                            -3.0
                        } else {
                            -5.0
                        },
                        1.5,
                        m_z,
                    ),
                ),
                (
                    "RRM",
                    Vector3::new(
                        if is_close_to_right && is_defensive_zone {
                            3.0
                        } else {
                            5.0
                        },
                        1.5,
                        m_z,
                    ),
                ),
                (
                    "LLD",
                    Vector3::new(
                        if is_close_to_left && is_defensive_zone {
                            -3.0
                        } else {
                            -5.0
                        },
                        1.5,
                        d_z,
                    ),
                ),
                (
                    "RRD",
                    Vector3::new(
                        if is_close_to_right && is_defensive_zone {
                            3.0
                        } else {
                            5.0
                        },
                        1.5,
                        d_z,
                    ),
                ),
                ("CM", Vector3::new(0.0, 1.5, m_z)),
                ("CD", Vector3::new(0.0, 1.5, d_z)),
                ("LW2", Vector3::new(-6.0, 1.5, winger_z)),
                ("RW2", Vector3::new(6.0, 1.5, winger_z)),
                (
                    "LLW",
                    Vector3::new(far_left_winger_x, 1.5, far_left_winger_z),
                ),
                (
                    "RRW",
                    Vector3::new(far_right_winger_x, 1.5, far_right_winger_z),
                ),
            ];
            for (s, offset) in offsets {
                let pos = center_position + rot * &offset;

                player_positions.insert(s, (pos, rot.clone()));
            }

            player_positions.insert("G", (goalie_pos.clone(), rot.clone()));

            player_positions
        }

        let red_player_positions = get_positions(
            &center_position,
            &red_rot,
            &red_goalie_pos,
            red_defensive_zone,
            red_left,
            red_right,
        );
        let blue_player_positions = get_positions(
            &center_position,
            &blue_rot,
            &blue_goalie_pos,
            blue_defensive_zone,
            blue_left,
            blue_right,
        );

        HQMFaceoffSpot {
            center_position,
            red_player_positions,
            blue_player_positions,
        }
    };

    match spot {
        HQMRinkFaceoffSpot::Center => create_faceoff_spot(Point3::new(center_x, 0.0, center_z)),
        HQMRinkFaceoffSpot::DefensiveZone(team, side) => {
            let z = match team {
                HQMTeam::Red => red_zone_faceoff_z,
                HQMTeam::Blue => blue_zone_faceoff_z,
            };
            let x = match side {
                HQMRinkSide::Left => left_faceoff_x,
                HQMRinkSide::Right => right_faceoff_x,
            };
            create_faceoff_spot(Point3::new(x, 0.0, z))
        }
        HQMRinkFaceoffSpot::Offside(team, side) => {
            let z = match team {
                HQMTeam::Red => red_neutral_faceoff_z,
                HQMTeam::Blue => blue_neutral_faceoff_z,
            };
            let x = match side {
                HQMRinkSide::Left => left_faceoff_x,
                HQMRinkSide::Right => right_faceoff_x,
            };
            create_faceoff_spot(Point3::new(x, 0.0, z))
        }
    }
}

#[derive(Eq, PartialEq, Debug, Copy, Clone)]
pub enum HQMSpawnPoint {
    Center,
    Bench,
}
pub fn get_spawnpoint(
    rink: &HQMRink,
    team: HQMTeam,
    spawn_point: HQMSpawnPoint,
) -> (Point3<f32>, Rotation3<f32>) {
    match team {
        HQMTeam::Red => match spawn_point {
            HQMSpawnPoint::Center => {
                let (z, rot) = ((rink.length / 2.0) + 3.0, 0.0);
                let pos = Point3::new(rink.width / 2.0, 2.0, z);
                let rot = Rotation3::from_euler_angles(0.0, rot, 0.0);
                (pos, rot)
            }
            HQMSpawnPoint::Bench => {
                let z = (rink.length / 2.0) + 4.0;
                let pos = Point3::new(0.5, 2.0, z);
                let rot = Rotation3::from_euler_angles(0.0, 3.0 * FRAC_PI_2, 0.0);
                (pos, rot)
            }
        },
        HQMTeam::Blue => match spawn_point {
            HQMSpawnPoint::Center => {
                let (z, rot) = ((rink.length / 2.0) - 3.0, PI);
                let pos = Point3::new(rink.width / 2.0, 2.0, z);
                let rot = Rotation3::from_euler_angles(0.0, rot, 0.0);
                (pos, rot)
            }
            HQMSpawnPoint::Bench => {
                let z = (rink.length / 2.0) - 4.0;
                let pos = Point3::new(0.5, 2.0, z);
                let rot = Rotation3::from_euler_angles(0.0, 3.0 * FRAC_PI_2, 0.0);
                (pos, rot)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::hqm_game::HQMTeam;
    use crate::hqm_match_util::setup_position;
    use crate::hqm_server::HQMServerPlayerIndex;
    use std::collections::HashMap;

    #[test]
    fn test1() {
        let c = "C";
        let lw = "LW";
        let rw = "RW";
        let g = "G";
        let mut res1 = HashMap::new();
        let players = vec![(HQMServerPlayerIndex(0), None)];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");

        let mut res1 = HashMap::new();
        let players = vec![(HQMServerPlayerIndex(0), Some(c))];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");

        let mut res1 = HashMap::new();
        let players = vec![(HQMServerPlayerIndex(0), Some(lw))];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");

        let mut res1 = HashMap::new();
        let players = vec![(HQMServerPlayerIndex(0), Some(g))];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");

        let mut res1 = HashMap::new();
        let players = vec![
            (HQMServerPlayerIndex(0usize), Some(c)),
            (HQMServerPlayerIndex(1), Some(lw)),
        ];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");
        assert_eq!(res1[&HQMServerPlayerIndex(1)].1, "LW");

        let mut res1 = HashMap::new();
        let players = vec![
            (HQMServerPlayerIndex(0), None),
            (HQMServerPlayerIndex(1), Some(lw)),
        ];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");
        assert_eq!(res1[&HQMServerPlayerIndex(1)].1, "LW");

        let mut res1 = HashMap::new();
        let players = vec![
            (HQMServerPlayerIndex(0), Some(rw)),
            (HQMServerPlayerIndex(1), Some(lw)),
        ];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");
        assert_eq!(res1[&HQMServerPlayerIndex(1)].1, "LW");

        let mut res1 = HashMap::new();
        let players = vec![
            (HQMServerPlayerIndex(0), Some(g)),
            (HQMServerPlayerIndex(1), Some(lw)),
        ];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "G");
        assert_eq!(res1[&HQMServerPlayerIndex(1)].1, "C");

        let mut res1 = HashMap::new();
        let players = vec![
            (HQMServerPlayerIndex(0usize), Some(c)),
            (HQMServerPlayerIndex(1), Some(c)),
        ];
        setup_position(&mut res1, players.as_ref(), HQMTeam::Red);
        assert_eq!(res1[&HQMServerPlayerIndex(0)].1, "C");
        assert_eq!(res1[&HQMServerPlayerIndex(1)].1, "LW");
    }
}
