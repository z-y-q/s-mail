//! AI 助手消息：摘要/起草/改写/智能回复/整理计划/隐私门/设置表单

use crate::update::update;
use crate::{ai, AiField, Message, Screen, State};
use iced::Task;

/// 消息组的统一入口（分发器按 or-pattern 委托；模式与处理体自 update.rs 原样搬运）
pub(crate) fn handle(state: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::AiSummarize(acct, hash) => ai::start_summarize(state, acct, hash),
        Message::AiSummaryReady {
            acct,
            hash,
            id,
            result,
        } => {
            ai::summary_ready(state, acct, hash, id, result);
            Task::none()
        }
        Message::AiSummaryClose => {
            state.ai.summary_visible = false;
            Task::none()
        }
        Message::AiDraftGo => {
            let instr = match &state.screen {
                Screen::Compose(c) => c.ai_instruction.clone(),
                _ => String::new(),
            };
            ai::start_draft(state, instr, None)
        }
        Message::AiDraftReady { id, result } => {
            ai::draft_ready(state, id, result);
            Task::none()
        }
        Message::AiDraftApply => {
            ai::draft_apply_overwrite(state);
            Task::none()
        }
        Message::AiDraftDiscard => {
            ai::draft_discard_overwrite(state);
            Task::none()
        }
        Message::AiInstructionInput(s) => {
            if let Screen::Compose(c) = &mut state.screen {
                c.ai_instruction = s;
            }
            Task::none()
        }
        Message::AiRewrite(instr) => ai::start_rewrite(state, instr),
        Message::AiDraftUndo => {
            ai::draft_undo(state);
            Task::none()
        }
        Message::AiSmartReply(acct, hash) => ai::smart_reply(state, acct, hash),
        Message::AiOrganizeGo => ai::organize_go(state),
        Message::AiPlanReady { id, result } => {
            ai::plan_ready(state, id, result);
            Task::none()
        }
        Message::AiPlanToggle(i) => {
            if let Some(groups) = &mut state.ai.plan {
                if let Some(g) = groups.get_mut(i) {
                    g.checked = !g.checked;
                }
            }
            Task::none()
        }
        Message::AiPlanExpand(i) => {
            if let Some(groups) = &mut state.ai.plan {
                if let Some(g) = groups.get_mut(i) {
                    g.expanded = !g.expanded;
                }
            }
            Task::none()
        }
        Message::AiPlanApply => {
            ai::plan_apply(state);
            Task::none()
        }
        Message::AiPlanCancel => {
            state.ai.plan = None;
            state.ai.plan_loading = false;
            state.ai.plan_ctx.clear();
            Task::none()
        }
        Message::AiPrivacyAck => {
            state.ai.ack = true;
            crate::lang::save_prefs(&state.data_root, &state.current_prefs());
            let pending = state.ai.privacy_pending.take();
            match pending {
                Some(m) => update(state, m),
                None => Task::none(),
            }
        }
        Message::AiPrivacyCancel => {
            state.ai.privacy_pending = None;
            Task::none()
        }
        Message::AiFormOpen => {
            state.ai.form = Some(ai::form_from_cfg(state));
            Task::none()
        }
        Message::AiFormClose => {
            state.ai.form = None;
            Task::none()
        }
        Message::AiFormPreset(p) => {
            if let Some(f) = &mut state.ai.form {
                f.preset = p;
                f.apply_preset();
            }
            Task::none()
        }
        Message::AiFormToggleEnabled(v) => {
            if let Some(f) = &mut state.ai.form {
                f.enabled = v;
            }
            Task::none()
        }
        Message::AiFormField(field, value) => {
            if let Some(f) = &mut state.ai.form {
                f.err = None;
                match field {
                    AiField::BaseUrl => f.base_url = value,
                    AiField::Model => f.model = value,
                    AiField::ApiKey => f.api_key = value,
                }
            }
            Task::none()
        }
        Message::AiFormKeychain => {
            ai::form_keychain_move(state);
            Task::none()
        }
        Message::AiFormTest => {
            let Some(f) = &state.ai.form else {
                return Task::none();
            };
            if f.base_url.trim().is_empty() || f.model.trim().is_empty() {
                let err = "AI 需要填 API 地址与模型".to_string();
                if let Some(f) = &mut state.ai.form {
                    f.err = Some(err);
                }
                return Task::none();
            }
            // 表单值优先，api_key 空则回落已存配置（钥匙串标记场景）
            let raw_key = if f.api_key.trim().is_empty() {
                state.ai.cfg.api_key.clone()
            } else {
                f.api_key.trim().to_string()
            };
            let (base_url, model) = (f.base_url.trim().to_string(), f.model.trim().to_string());
            let chat = match if smail_core::keychain::is_marker(&raw_key) {
                smail_core::keychain::resolve(&raw_key)
            } else {
                Ok(raw_key)
            } {
                Ok(k) => smail_ai::Chat {
                    base_url,
                    api_key: k,
                    model,
                },
                Err(e) => {
                    if let Some(f) = &mut state.ai.form {
                        f.err = Some(e);
                    }
                    return Task::none();
                }
            };
            if let Some(f) = &mut state.ai.form {
                f.testing = true;
                f.test_result = None;
            }
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || chat.ping())
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                },
                Message::AiFormTestReady,
            )
        }
        Message::AiFormTestReady(res) => {
            if let Some(f) = &mut state.ai.form {
                f.testing = false;
                f.test_result = Some(res);
            }
            Task::none()
        }
        Message::AiFormSave => {
            ai::form_save(state);
            Task::none()
        }
        _ => unreachable!("update 分发器把非本组消息路由到了 ai_ops：{msg:?}"),
    }
}
