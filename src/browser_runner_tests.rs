//! In-memory CDP exchanges exercise the native sequence. They do not execute DOM JavaScript.
use super::*;
use crate::config::Environment;
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

#[derive(Clone, Copy, Debug, Default)]
enum Fault {
    UploadCleanupWrongId,
    UploadCleanupWrongOwner,
    UploadCleanupWrongNavigation,
    #[default]
    None,
    CookieBootstrap,
    CookieRejected,
    CookieCancelled,
    CookieJournalFailure,
    RemoteCapture,
    RemoteCaptureRejected,
    AttemptParentBeforeIntent,
    AttemptParentBeforeSend,
    InitialTargetInspection,
    WrongModel,
    WrongTier,
    GenericUnavailable,
    ResponseLoadFailed,
    ResponseLoadFailedBesideImage,
    ContentWithheld,
    RetryOffered,
    ProviderLateCommit,
    ProviderLateCommitPendingAnswer,
    ProviderLateCommitLostAck,
    ProviderLateCommitChangedOwner,
    ProviderLateAnswer,
    ProviderLateAnswerDeadline,
    ProviderRecoveryWrongSource,
    ProviderRecoveryChangedRequest,
    ProviderRecoveryChangedOwner,
    ProviderRecoveryCancelled,
    ProviderRecoveryPrivacy,
    RecoveryParentCancelledAtReceipt,
    RecoveryParentCancelledAtAnswer,
    RecoveryParentCancelledAtRetirement,
    RecoveryObserverCancelledAtRetirement,
    RecoveryRetryParentCancelledAtRetirement,
    RecoveryRetryModelCancelledAtRetirement,
    RecoveryRetryObserverCancelledAtRetirement,
    DisabledPro,
    DisabledProForeign,
    // GPT-6's top Power step also checks the Pro model radio (27 October 2026).
    ProModel,
    ProModelExtraHigh,
    ProModelLegacyRow,
    LegacyRowPro,
    ChangedModelOwner,
    SequentialProModel,
    SequentialExtraHighProModel,
    DisabledExtraHigh,
    ModelThrottled,
    ModelsNotLoaded,
    ModelPending,
    SendSettling,
    SendNeverReady,
    SendAttachmentsNeverReady,
    ChatListLimited,
    MissingSource,
    MissingWitness,
    ForeignWitness,
    ConflictingWitness,
    WrongSource,
    HistoricalEcho,
    Followup,
    Borrowed,
    BorrowedWork,
    BorrowedWrongTier,
    BorrowedClaimed,
    BorrowedPartialPrompt,
    BorrowedSecondUploadPrepareFailed,
    BorrowedUploadAckFailed,
    BorrowedReleaseFailed,
    LegacyFollowup,
    FollowupExtraHigh,
    FollowupWrongModel,
    FollowupWrongTier,
    Sequential,
    SequentialExtraHigh,
    SequentialWrongModel,
    SequentialWrongTier,
    SequentialChangedPrior,
    SequentialLostAck,
    SequentialCancel,
    SequentialTimeout,
    SequentialFile,
    SequentialImage,
    SequentialLastLostAck,
    SequentialNamedAll,
    SequentialNamedFirst,
    SequentialNamedLast,
    SequentialNamedButtons,
    SequentialNamedBad,
    SequentialNamedBadButtons,
    SequentialNamedDelayed,
    SequentialNamedFirstThenFile,
    SequentialPrivacySettling,
    SequentialPrivacyUnknown,
    SequentialPrivacyOff,
    UploadFailed,
    SecondUploadFailed,
    UploadCleanupFailed,
    UploadObjectUnacknowledged,
    UploadDescribeUnacknowledged,
    UploadCommandUnacknowledged,
    CleanupFailed,
    PreDispatchForeignMarker,
    PreDispatchLeaseReplacement,
    PreDispatchQuarantineFailed,
    PreDispatchCloseRefused,
    CancelPickerDefault,
    CancelPickerKeep,
    CancelPickerKeepClose,
    CancelPickerRefuse,
    LostSendAck,
    OrphanModelError,
    CancelSend,
    CancelSendGenerating,
    CancelSendCloseRefused,
    ReplacedOwner,
    ReplacedOutcomeTarget,
    ChangedOutcomeOptions,
    StoppedOutcomeModel,
    CancelOutcome,
    TargetDuringDownload,
    ModelDuringDownload,
    PartialDuringDownload,
    CancelledDuringDownload,
    OptionsDuringDownload,
    CancelDuringDownload,
    NewerReceipt,
    CleanupRetry,
    CleanupCleanRetry,
    AnswerDuringRetirement,
    SourceDuringRetirement,
    RecoveryAnswerDuringRetirement,
    RecoverySourceDuringRetirement,
    UncertainClose,
    CloseRefusedOnce,
    CloseRefusedTwice,
    CloseRefusedThenLockedSettlement,
    CloseRefusedForeignMarker,
    CloseRefusedChangedSource,
    CloseRefusedActiveThinking,
    CloseRefusedMissingCompletion,
    ForeignRetirement,
    Incomplete,
    File,
    FileExport,
    FileExportNoAnswer,
    FileExportChangedSource,
    FileExportFormatted,
    FormattedMarkdown,
    FormattedImage,
    TwoFilePartial,
    Image,
    ImageOnly,
    NamedImage,
    OutputImage,
    MultiImage,
    DuplicateImage,
    PartialImage,
    FirstBadMultiImage,
    DelayedBadImage,
    ButtonOutputFailure,
    InvalidDirectButtonImage,
    ExistingImage,
    MissingImage,
    DelayedImage,
    ButtonImage,
    LateButtonImage,
    MultiButtonImage,
    MultiButtonLostImage,
    NonImageButton,
    NonImageThenImage,
    BadImage,
    ForeignDownloadFrame,
    DuplicateDownloadGuid,
    LostDownloadAck,
    TruncatedComposer,
    SmallComposerMismatch,
    PromptCleanupFailed,
    PromptRollbackFailed,
    Cloudflare,
    SignedOut,
    ManualLoginLate,
    ManualLoginTimeout,
    OwnedWork,
    UiWarningTimeout,
    ImageUiWarning,
    StaleConversationCookie,
}
struct Exchange {
    fault: Fault,
    prompt: String,
    url: String,
    owner: Value,
    commit: Value,
    before: bool,
    turns: Vec<String>,
    sends: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    closed: bool,
    trace: Arc<Mutex<Vec<Value>>>,
    cancel: CancellationToken,
    store: SessionStore,
    id: String,
    download_stage: Option<std::path::PathBuf>,
    result_reads: usize,
    recovery_observed: bool,
    download_count: usize,
    delayed_image_visible: bool,
    retirement_marker: Option<Value>,
    pending_retirement_read: bool,
    cleanup_lock: Option<DirectoryLock>,
}
impl Exchange {
    /// Reference page gates and notices (browser_page.rs scripts).
    fn page_gate(&self, expression: &str) -> Option<Value> {
        let count = |marker: &str| {
            self.trace
                .lock()
                .unwrap()
                .iter()
                .filter(|r| {
                    r["params"]["expression"]
                        .as_str()
                        .is_some_and(|e| e.contains(marker))
                        || r["params"]["type"] == marker
                })
                .count()
        };
        if expression.contains("industrious-dom:blocked") {
            let challenge = matches!(self.fault, Fault::Cloudflare);
            return Some(
                json!({"strong":challenge,"shell":!challenge,"weak":false,"securityBlock":false}),
            );
        }
        if expression.contains("industrious-dom:health-login") {
            let ok = match self.fault {
                Fault::SignedOut | Fault::ManualLoginTimeout => false,
                Fault::ManualLoginLate => count("industrious-dom:health-login") > 2,
                _ => true,
            };
            return Some(
                json!({"ok":ok,"status":if ok {200} else {401},"backendStatus":null,"domLoginCta":!ok,"onAuthPage":false,"appAuthenticated":ok,"cfBlocked":false,"sessionAuthenticated":ok,"sessionResolved":true,"pageUrl":self.url,"error":null}),
            );
        }
        if expression.contains("industrious-dom:chat-mode-target") {
            return Some(json!(true));
        }
        if expression.contains("industrious-dom:chat-mode") {
            return Some(match self.fault {
                Fault::OwnedWork if count("mouseReleased") == 0 => {
                    json!({"status":"work-selected","chatPoint":{"x":40.5,"y":20}})
                }
                Fault::OwnedWork => json!({"status":"chat-selected"}),
                _ => json!({"status":"controls-absent"}),
            });
        }
        if expression.contains("industrious-dom:ui-warnings") {
            return Some(
                if matches!(self.fault, Fault::UiWarningTimeout | Fault::ImageUiWarning) {
                    json!([{"text":"  Too many requests.
 Contact PUBLIC@example.com  ","source":"selector","role":"alert","ariaLive":null,"selector":"[role=\"alert\"]"}])
                } else {
                    json!([])
                },
            );
        }
        None
    }
    /// Live predicate reads since the fake last remounted the conversation view: at each
    /// send, and as each later sequential turn reacquires its renderer. Includes this read.
    fn privacy_reads_since_remount(&self) -> usize {
        self.trace
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take_while(|r| {
                !(r["method"] == "Input.dispatchKeyEvent" && r["params"]["type"] == "keyDown")
                    && !r["params"]["expression"].as_str().is_some_and(|e| {
                        e.contains("industrious-dom:submission")
                            && e.contains("\"action\":\"acquire\"")
                    })
            })
            .filter(|r| {
                r["params"]["expression"]
                    .as_str()
                    .is_some_and(|e| e.contains("industrious-dom:temporary-chat"))
            })
            .count()
    }
    /// The live predicate after the first send: `null` whilst the view is unmounted.
    /// A settling view proves Temporary Chat again on its third read.
    fn sequential_privacy(&self) -> Option<Value> {
        if self.sends.load(Ordering::SeqCst) == 0 {
            return None;
        }
        match self.fault {
            Fault::SequentialPrivacySettling if self.privacy_reads_since_remount() <= 2 => {
                Some(Value::Null)
            }
            Fault::SequentialPrivacySettling => Some(json!(true)),
            Fault::SequentialPrivacyUnknown => Some(Value::Null),
            Fault::SequentialPrivacyOff => Some(json!(false)),
            _ => None,
        }
    }
    /// A `browser_privacy::guard` script throws in the page unless the predicate is true now.
    fn guard_refuses(&self, expression: &str) -> bool {
        expression.contains("if (!oracleTemporaryChat()) throw Error(")
            && self.sends.load(Ordering::SeqCst) > 0
            && match self.fault {
                Fault::SequentialPrivacySettling => self.privacy_reads_since_remount() < 3,
                Fault::SequentialPrivacyUnknown | Fault::SequentialPrivacyOff => true,
                _ => false,
            }
    }
    fn provider_recovery_started(&self) -> bool {
        self.store.require(&self.id).unwrap()["browser"]["runtime"]["providerReadRecovery"]["version"]
            == 1
    }
    fn borrowed(&self) -> bool {
        matches!(
            self.fault,
            Fault::Borrowed
                | Fault::BorrowedWork
                | Fault::BorrowedWrongTier
                | Fault::BorrowedClaimed
                | Fault::BorrowedPartialPrompt
                | Fault::BorrowedSecondUploadPrepareFailed
                | Fault::BorrowedUploadAckFailed
                | Fault::BorrowedReleaseFailed
        )
    }
    fn sequential(&self) -> bool {
        matches!(
            self.fault,
            Fault::Sequential
                | Fault::SequentialExtraHigh
                | Fault::SequentialWrongModel
                | Fault::SequentialWrongTier
                | Fault::SequentialChangedPrior
                | Fault::SequentialLostAck
                | Fault::SequentialCancel
                | Fault::SequentialTimeout
                | Fault::SequentialFile
                | Fault::SequentialImage
                | Fault::SequentialLastLostAck
                | Fault::SequentialNamedAll
                | Fault::SequentialNamedFirst
                | Fault::SequentialNamedLast
                | Fault::SequentialNamedButtons
                | Fault::SequentialNamedBad
                | Fault::SequentialNamedBadButtons
                | Fault::SequentialNamedDelayed
                | Fault::SequentialNamedFirstThenFile
                | Fault::SequentialPrivacySettling
                | Fault::SequentialPrivacyUnknown
                | Fault::SequentialPrivacyOff
                | Fault::SequentialProModel
                | Fault::SequentialExtraHighProModel
        )
    }
    /// The model radio the picker shows after effort selection for the Pro-model faults.
    fn pro_model_checked(&self) -> bool {
        let selected = |level: &str| {
            self.trace.lock().unwrap().iter().any(|r| {
                r["params"]["expression"].as_str().is_some_and(|s| {
                    s.contains("industrious-dom:thinking")
                        && s.contains("\"inspectionOnly\":false")
                        && s.contains(level)
                })
            })
        };
        match self.fault {
            Fault::ProModel | Fault::SequentialProModel | Fault::ProModelLegacyRow => {
                selected("\"level\":\"pro\"")
            }
            Fault::ProModelExtraHigh => selected("\"level\":"),
            Fault::SequentialExtraHighProModel => self.sends.load(Ordering::SeqCst) > 0,
            _ => false,
        }
    }
    fn probe(&self, before: bool) -> Value {
        if self.sequential() {
            let messages=self.turns.iter().enumerate().map(|(i,prompt)|json!({"id":format!("user-public-{}",i+1),"turnId":format!("user-turn-public-{}",i+1),"index":i*2,"matches":*prompt==self.prompt,"fingerprintText":prompt})).collect::<Vec<_>>();
            return json!({"currentUrl":self.url,"workSelected":false,"modeUnverified":false,"invalid":false,"turnIds":self.turns.iter().enumerate().flat_map(|(i,_)|[format!("user-turn-public-{}",i+1),format!("assistant-turn-public-{}",i+1)]).collect::<Vec<_>>(),"messages":messages,"canonicalPrompt":self.prompt});
        }
        let historical = matches!(self.fault, Fault::HistoricalEcho);
        let messages = if before && !historical {
            vec![]
        } else {
            vec![
                json!({"id":"user-public","turnId":"user-turn-public","index":0,"matches":!matches!(self.fault, Fault::MissingSource | Fault::WrongSource),"fingerprintText":if matches!(self.fault, Fault::MissingSource) {Value::Null} else if matches!(self.fault, Fault::WrongSource) {json!("PUBLIC wrong original source")} else {json!(self.prompt)}}),
            ]
        };
        json!({"currentUrl":if before && !historical {"https://chatgpt.com/"} else {"https://chatgpt.com/c/consult-public"},"workSelected":false,"modeUnverified":false,"invalid":false,"turnIds":if before && !historical {vec![]} else {vec!["user-turn-public"]},"messages":messages,"canonicalPrompt":self.prompt})
    }
    fn result(&self) -> Value {
        if self.sequential() {
            let count = self.turns.len();
            let images = matches!(
                self.fault,
                Fault::SequentialNamedAll | Fault::SequentialNamedBad
            ) || count == 1
                && matches!(
                    self.fault,
                    Fault::SequentialImage
                        | Fault::SequentialNamedFirst
                        | Fault::SequentialNamedFirstThenFile
                )
                || count == 3
                    && (matches!(self.fault, Fault::SequentialNamedLast)
                        || matches!(self.fault, Fault::SequentialNamedDelayed)
                            && self.delayed_image_visible);
            return json!({"currentUrl":self.url,"conversationId":"consult-public","origin":"https://chatgpt.com","lastUserMessageId":format!("user-public-{count}"),"lastUserTurnIndex":(count-1)*2,"lastUserOriginalSourceMessageId":format!("user-public-{count}"),"lastUserOriginalSourceText":self.turns.last(),"lastAssistantMessageId":format!("assistant-public-{count}"),"lastAssistantTurnId":format!("assistant-turn-public-{count}"),"lastAssistantTurnIndex":count*2-1,"assistantFollowsLatestUser":true,"stopExists":false,"strongThinkingActive":false,"completionVisible":!(count==2&&matches!(self.fault,Fault::SequentialTimeout)),"text":format!("PUBLIC owned answer {count}"),"html":format!("<p>PUBLIC owned answer {count}</p>"),"images":if images{vec![json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public","alt":"Generated image","width":12,"height":12})]}else{vec![]},"files":if count==1&&matches!(self.fault,Fault::SequentialFile)||count==2&&matches!(self.fault,Fault::SequentialNamedFirstThenFile){vec![json!({"url":"https://chatgpt.com/backend-api/files/file_public/download","downloadUrl":"https://chatgpt.com/backend-api/files/file_public/download","filename":"PUBLIC.txt","candidateId":"","label":"Download PUBLIC.txt"})]}else{vec![]}});
        }
        let files = matches!(
            self.fault,
            Fault::File
                | Fault::TwoFilePartial
                | Fault::FileExport
                | Fault::FileExportNoAnswer
                | Fault::FileExportChangedSource
                | Fault::FileExportFormatted
                | Fault::ForeignDownloadFrame
                | Fault::DuplicateDownloadGuid
                | Fault::LostDownloadAck
        );
        let images = matches!(
            self.fault,
            Fault::Image
                | Fault::FormattedImage
                | Fault::BadImage
                | Fault::ImageOnly
                | Fault::NamedImage
                | Fault::OutputImage
                | Fault::MultiImage
                | Fault::DuplicateImage
                | Fault::PartialImage
                | Fault::FirstBadMultiImage
                | Fault::InvalidDirectButtonImage
                | Fault::ExistingImage
                | Fault::TargetDuringDownload
                | Fault::ModelDuringDownload
                | Fault::PartialDuringDownload
                | Fault::CancelledDuringDownload
                | Fault::OptionsDuringDownload
                | Fault::CancelDuringDownload
                | Fault::NewerReceipt
        );
        json!({"currentUrl":"https://chatgpt.com/c/consult-public","conversationId":"consult-public","origin":"https://chatgpt.com","lastUserMessageId":"user-public","lastUserTurnIndex":0,"lastUserOriginalSourceMessageId":"user-public","lastUserOriginalSourceText":self.prompt,"lastAssistantMessageId":"assistant-public","lastAssistantTurnId":"assistant-turn-public","lastAssistantTurnIndex":1,"assistantFollowsLatestUser":true,"stopExists":false,"strongThinkingActive":false,"completionVisible":!matches!(self.fault, Fault::Incomplete | Fault::UiWarningTimeout),"text":if matches!(self.fault,Fault::ImageOnly){""}else if matches!(self.fault,Fault::ResponseLoadFailed){"This response couldn’t load"}else{"PUBLIC exact owned answer\n\nEnd\n"},"html":"<p>PUBLIC exact owned answer</p><p>End</p>","images":if images{vec![json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public","alt":"Generated image","width":12,"height":12})]}else{vec![]},"files":if matches!(self.fault,Fault::TwoFilePartial){vec![json!({"url":"browser-download","filename":"PUBLIC-first.txt","candidateId":"1:0","label":"Download PUBLIC first"}),json!({"url":"browser-download","filename":"PUBLIC-second.txt","candidateId":"1:1","label":"Download PUBLIC second"})]}else if files{vec![json!({"url":"https://chatgpt.com/backend-api/files/file_public/download","downloadUrl":"https://chatgpt.com/backend-api/files/file_public/download","filename":"PUBLIC.txt","candidateId":"","label":"Download PUBLIC.txt"})]}else{vec![]}})
    }
    fn evaluate(&mut self, expression: &str) -> Value {
        if expression == "location.href" {
            return json!(self.url);
        }
        if expression == "document.readyState" {
            return json!("complete");
        }
        if expression.contains("industrious-dom:unused-blank") {
            return json!(self.url == "about:blank");
        }
        let args: Value = expression
            .rsplit_once(")(...")
            .and_then(|(_, suffix)| suffix.strip_suffix(')'))
            .and_then(|s| serde_json::from_str(s).ok())
            .expect("Browser arguments must be serialised JSON");
        let cfg = &args[0];
        if let Some(value) = self.page_gate(expression) {
            return value;
        }
        if expression.contains("industrious-dom:pending-retirement") {
            if matches!(
                self.fault,
                Fault::RecoveryRetryParentCancelledAtRetirement
                    | Fault::RecoveryRetryModelCancelledAtRetirement
                    | Fault::RecoveryRetryObserverCancelledAtRetirement
            ) {
                let child = self.store.require(&self.id).unwrap();
                let parent_id = child["browser"]["attempt"]["parentId"].as_str().unwrap();
                let parent = self.store.require(parent_id).unwrap();
                if !parent["cleanupWarning"].is_string() {
                    return json!("foreign");
                }
                match self.fault {
                    Fault::RecoveryRetryParentCancelledAtRetirement => {
                        self.store
                            .update(parent_id, |_| Ok(Some(json!({"status":"cancelled"}))))
                            .unwrap();
                    }
                    Fault::RecoveryRetryModelCancelledAtRetirement => {
                        self.store
                            .update_session_and_model(
                                parent_id,
                                parent["model"].as_str().unwrap(),
                                |_, _| Ok(Some((json!({}), Some(json!({"status":"cancelled"}))))),
                            )
                            .unwrap();
                    }
                    _ => self.cancel.cancel(),
                }
            }
            if matches!(self.fault, Fault::RecoveryParentCancelledAtRetirement) {
                let child = self.store.require(&self.id).unwrap();
                self.store
                    .update(
                        child["browser"]["attempt"]["parentId"].as_str().unwrap(),
                        |_| Ok(Some(json!({"status":"cancelled"}))),
                    )
                    .unwrap();
            }
            if matches!(self.fault, Fault::RecoveryObserverCancelledAtRetirement) {
                self.cancel.cancel();
            }
            self.pending_retirement_read = true;
            return json!(if self.retirement_marker.is_none() {
                "unmarked"
            } else if matches!(self.fault, Fault::CloseRefusedForeignMarker)
                || self.retirement_marker.as_ref() != Some(&cfg["retirementId"])
            {
                "foreign"
            } else {
                "marked"
            });
        }
        if expression.contains("industrious-dom:submission-receipt") {
            if matches!(self.fault, Fault::RecoveryParentCancelledAtReceipt) {
                let child = self.store.require(&self.id).unwrap();
                self.store
                    .update(
                        child["browser"]["attempt"]["parentId"].as_str().unwrap(),
                        |_| Ok(Some(json!({"status":"cancelled"}))),
                    )
                    .unwrap();
            }
            self.prompt = cfg["prompt"].as_str().unwrap().into();
            let probe = self.probe(false);
            if matches!(
                self.fault,
                Fault::ProviderLateCommit
                    | Fault::ProviderLateCommitPendingAnswer
                    | Fault::ProviderLateCommitLostAck
                    | Fault::ProviderLateCommitChangedOwner
            ) && !self.provider_recovery_started()
            {
                return json!({"probe":probe,"submission":{"state":"pending"}});
            }
            let submission = if !cfg["saved"].is_null() {
                json!({"state":"captured","witness":cfg["saved"]})
            } else if matches!(self.fault, Fault::MissingWitness) {
                json!({"state":"unavailable"})
            } else if matches!(self.fault, Fault::ConflictingWitness) {
                json!({"state":"invalid","reason":"multiple-submission-requests"})
            } else {
                json!({"state":"captured","witness":{"version":1,
                    "owner":if matches!(self.fault, Fault::ForeignWitness){json!({"claimId":"PUBLIC-foreign","reservationId":"PUBLIC-foreign"})}else{cfg["owner"].clone()},
                    "witnessId":cfg["intent"]["witnessId"],"dispatchGuardId":cfg["intent"]["dispatchGuardId"],
                    "promptSha256":cfg["intent"]["promptSha256"],"messageId":probe["messages"].as_array().unwrap().last().unwrap()["id"],
                    "source":"chatgpt-fetch-v1","requestPath":"/backend-api/f/conversation","historyDisabled":true}})
            };
            return json!({"probe":probe,"submission":submission});
        }
        if expression.contains("const previousIds = ids") {
            self.prompt = args[2].as_str().unwrap().into();
            self.before = args[1].is_null();
            return self.probe(self.before);
        }
        if expression.contains("industrious-dom:submission-witness-clean") {
            return json!(true);
        }
        if expression.contains("industrious-dom:image-buttons") {
            if matches!(self.fault, Fault::SequentialNamedFirstThenFile) && self.turns.len() == 2 {
                return json!([{"url":"https://chatgpt.com/backend-api/files/file_public/download","candidateId":"3:0","filename":"PUBLIC.txt","label":"Download PUBLIC.txt","imageButton":true,"buttonReceiptId":cfg["buttonReceiptId"]}]);
            }
            if matches!(self.fault, Fault::SequentialNamedDelayed) && self.turns.len() == 3 {
                self.delayed_image_visible = true;
            }
            let present = matches!(
                self.fault,
                Fault::ButtonImage
                    | Fault::MultiButtonImage
                    | Fault::MultiButtonLostImage
                    | Fault::NonImageButton
                    | Fault::NonImageThenImage
                    | Fault::InvalidDirectButtonImage
                    | Fault::ButtonOutputFailure
                    | Fault::SequentialNamedButtons
                    | Fault::SequentialNamedBadButtons
            ) || matches!(self.fault, Fault::LateButtonImage)
                && self.result_reads >= 6;
            if !present {
                return json!([]);
            }
            let mut buttons = vec![
                json!({"url":"browser-download","candidateId":"1:0","filename":"Download","label":"Download","imageButton":true,"buttonReceiptId":cfg["buttonReceiptId"]}),
            ];
            if matches!(
                self.fault,
                Fault::MultiButtonImage | Fault::MultiButtonLostImage
            ) && self.download_count < 2
            {
                buttons.push(json!({"url":"browser-download","candidateId":"1:1","filename":"Download 2","label":"Download 2","imageButton":true,"buttonReceiptId":cfg["buttonReceiptId"]}));
            }
            return json!(buttons);
        }
        if expression.contains("industrious-dom:copy-markdown") {
            if self.sequential() {
                return json!({"success":true,"markdown":format!("PUBLIC owned answer {}",self.turns.len())});
            }
            if matches!(self.fault, Fault::ReplacedOutcomeTarget) {
                self.store.update(&self.id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"chromeTargetId":"REPLACED-PUBLIC","ownedRecoveryTarget":merge_object(&current["browser"]["runtime"]["ownedRecoveryTarget"],&json!({"targetId":"REPLACED-PUBLIC"}))}))}))})))).unwrap();
            } else if matches!(self.fault, Fault::ChangedOutcomeOptions) {
                self.store.update(&self.id,|current|Ok(Some(json!({"options":merge_object(&current["options"],&json!({"writeOutputPath":std::path::Path::new(current["cwd"].as_str().unwrap()).join("PUBLIC-foreign-output.md")}))})))).unwrap();
            } else if matches!(self.fault, Fault::StoppedOutcomeModel) {
                let model = self.store.require(&self.id).unwrap()["model"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                self.store
                    .update_model(
                        &self.id,
                        &model,
                        &json!({"status":"partial","marker":"PUBLIC winning model outcome"}),
                    )
                    .unwrap();
            } else if matches!(self.fault, Fault::CancelOutcome) {
                self.cancel.cancel();
            }
            if matches!(self.fault, Fault::ImageOnly) {
                return json!({"success":true,"markdown":""});
            }
            return json!({"success":true,"markdown":if matches!(self.fault,Fault::FileExportFormatted | Fault::FormattedMarkdown | Fault::FormattedImage){"**PUBLIC exact owned answer**\n\nEnd\n"}else{"PUBLIC exact owned answer\n\nEnd\n"}});
        }
        if expression.contains("return claim.submission.commitReceipt;") {
            return self.commit.clone();
        }
        if expression.contains("industrious-dom:download-click") {
            return json!({"clicked":1});
        }
        if expression.contains("industrious-dom:submission") {
            self.owner = cfg["owner"].clone();
            if cfg["action"] == "dispatch" && matches!(self.fault, Fault::AttemptParentBeforeSend) {
                let child = self.store.require(&self.id).unwrap();
                let parent = child["browser"]["attempt"]["parentId"].as_str().unwrap();
                self.store
                    .update(parent, |_| Ok(Some(json!({"status":"cancelled"}))))
                    .unwrap();
            }
            if cfg["action"] == "acquire" && matches!(self.fault, Fault::BorrowedClaimed) {
                return json!(false);
            }
            if cfg["action"] == "quarantine"
                && matches!(self.fault, Fault::PreDispatchQuarantineFailed)
            {
                return json!(false);
            }
            if cfg["action"] == "release" && matches!(self.fault, Fault::BorrowedReleaseFailed) {
                return json!(false);
            }
            if cfg["action"] == "acknowledge" {
                self.commit = cfg["receipt"].clone();
            }
            return json!(true);
        }
        if expression.contains("industrious-dom:predispatch-marker") {
            assert_eq!(cfg["owner"], self.owner);
            let creator =
                self.store.require(&self.id).unwrap()["browser"]["runtime"]["ownedRecoveryTarget"]
                    ["creator"]
                    .clone();
            let record = TargetJournal::open(&self.store)
                .unwrap()
                .read(&creator["allocationId"])
                .unwrap();
            assert_eq!(
                record["disposition"]["reason"],
                "settled-pre-dispatch-close-pending"
            );
            assert_eq!(record["disposition"]["absence"], "unknown");
            assert_eq!(record["preDispatchRetirement"]["id"], cfg["retirementId"]);
            if let Some(previous) = &self.retirement_marker {
                assert_eq!(
                    *previous, cfg["retirementId"],
                    "A retry must resume its saved marker"
                );
            }
            self.retirement_marker = Some(cfg["retirementId"].clone());
            if matches!(self.fault, Fault::PreDispatchLeaseReplacement) {
                let path = std::path::Path::new(creator["lease"]["profileDir"].as_str().unwrap())
                    .join("oracle-tab-leases.json");
                let mut registry: Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                registry["leases"][0]["sessionId"] = json!("PUBLIC replacement lease");
                crate::storage::atomic_json(&path, &registry).unwrap();
            }
            return json!(true);
        }
        if expression.contains("industrious-dom:predispatch-inspect") {
            assert_eq!(cfg["owner"], self.owner);
            return json!(!matches!(self.fault, Fault::PreDispatchForeignMarker));
        }
        if expression.contains("industrious-dom:health-guard") {
            return match self.fault {
                Fault::ModelThrottled => {
                    json!({"throttled":true,"modelsLoadFailed":false,"throttleMessage":"Too many requests. PUBLIC notice","modelsMessage":null,"modelsWhere":null})
                }
                Fault::ModelsNotLoaded => {
                    json!({"throttled":false,"modelsLoadFailed":true,"throttleMessage":null,"modelsMessage":"Could not load ChatGPT models","modelsWhere":"model-button"})
                }
                _ => {
                    json!({"throttled":false,"modelsLoadFailed":false,"throttleMessage":null,"modelsMessage":null,"modelsWhere":null})
                }
            };
        }
        if expression.contains("industrious-dom:inspection-prepare") {
            return json!({"token":cfg["token"],"parentReservationId":cfg["owner"]["reservationId"],"rendererId":"renderer-public","acknowledged":true});
        }
        if expression.contains("industrious-dom:inspection-finish") {
            if matches!(
                self.fault,
                Fault::CancelPickerDefault
                    | Fault::CancelPickerKeep
                    | Fault::CancelPickerKeepClose
                    | Fault::CancelPickerRefuse
            ) {
                self.cancel.cancel();
            }
            return json!({"token":cfg["token"],"parentReservationId":cfg["owner"]["reservationId"],"rendererId":cfg["rendererId"],"acknowledged":true,"cleanupComplete":!matches!(self.fault, Fault::CleanupFailed)});
        }
        if expression.contains("industrious-dom:model") {
            if cfg["strategy"] == "select" {
                let first = self
                    .trace
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| {
                        r["params"]["expression"]
                            .as_str()
                            .is_some_and(|s| s.contains("industrious-dom:model"))
                    })
                    .count()
                    == 1;
                match self.fault {
                    Fault::ModelThrottled | Fault::ModelsNotLoaded => {
                        return json!({"status":"option-not-found"});
                    }
                    Fault::ModelPending if first => return json!({"status":"pending"}),
                    _ => {}
                }
            }
            if self.sequential() && self.sends.load(Ordering::SeqCst) > 0 {
                assert_eq!(
                    cfg["inspectCurrent"], "evidence",
                    "Sequential turn selected a model"
                );
                if matches!(self.fault, Fault::SequentialWrongModel) {
                    return json!({"status":"already-selected","label":"GPT-5.6 Sol","checkedCount":1,"ownerKey":"model-trigger-public"});
                }
            }
            if self.borrowed()
                || matches!(
                    self.fault,
                    Fault::Followup
                        | Fault::LegacyFollowup
                        | Fault::FollowupExtraHigh
                        | Fault::FollowupWrongModel
                        | Fault::FollowupWrongTier
                )
            {
                assert_eq!(
                    cfg["inspectCurrent"], "evidence",
                    "Follow-up selected a model"
                );
            }
            if matches!(self.fault, Fault::ChangedModelOwner)
                && self.trace.lock().unwrap().iter().any(|r| {
                    r["params"]["expression"].as_str().is_some_and(|s| {
                        s.contains("industrious-dom:thinking")
                            && s.contains("\"inspectionOnly\":false")
                    })
                })
            {
                return json!({"status":"already-selected","label":"GPT-6","checkedCount":1,"ownerKey":"model-trigger-replaced"});
            }
            if self.pro_model_checked() {
                return json!({"status":"already-selected","label":"Pro","checkedCount":1,"ownerKey":"model-trigger-public"});
            }
            return json!({"status":"already-selected","label":if matches!(self.fault, Fault::WrongModel|Fault::FollowupWrongModel|Fault::PreDispatchForeignMarker|Fault::PreDispatchLeaseReplacement|Fault::PreDispatchQuarantineFailed|Fault::PreDispatchCloseRefused|Fault::CancelPickerDefault|Fault::CancelPickerKeep|Fault::CancelPickerKeepClose|Fault::CancelPickerRefuse) {"GPT-5.6 Sol"} else {"GPT-6"},"checkedCount":1,"ownerKey":"model-trigger-public"});
        }
        if expression.contains("industrious-dom:thinking") {
            if self.sequential() && self.sends.load(Ordering::SeqCst) > 0 {
                assert_eq!(
                    cfg["inspectionOnly"], true,
                    "Sequential turn selected a thinking effort"
                );
                if matches!(self.fault, Fault::SequentialWrongTier) {
                    return json!({"status":"ready","selectedLevel":"extra-high","ownerKey":"model-trigger-public","basis":"checked-tier-row"});
                }
            }
            if self.borrowed()
                || matches!(
                    self.fault,
                    Fault::Followup
                        | Fault::LegacyFollowup
                        | Fault::FollowupExtraHigh
                        | Fault::FollowupWrongModel
                        | Fault::FollowupWrongTier
                )
            {
                assert_eq!(
                    cfg["inspectionOnly"], true,
                    "Follow-up selected a thinking effort"
                );
            }
            if cfg["inspectionOnly"] == true {
                return json!({"status":"ready","selectedLevel":if matches!(self.fault, Fault::WrongTier | Fault::BorrowedWrongTier) {json!("extended")}else if matches!(self.fault,Fault::FollowupWrongTier){json!("extra-high")} else {cfg["level"].clone()},"ownerKey":"model-trigger-public","basis":if cfg["level"] == "pro" && !matches!(self.fault, Fault::ProModelLegacyRow | Fault::LegacyRowPro) {"slider-label-and-index"} else {"checked-tier-row"}});
            }
            if cfg["level"] == "pro"
                && matches!(
                    self.fault,
                    Fault::DisabledPro
                        | Fault::SequentialExtraHigh
                        | Fault::DisabledExtraHigh
                        | Fault::ProModelExtraHigh
                        | Fault::SequentialExtraHighProModel
                )
            {
                return json!({"status":"option-disabled","label":"Pro","unavailableModel":"GPT-6","notice":"PUBLIC disabled row"});
            }
            if matches!(self.fault, Fault::DisabledProForeign) {
                return json!({"status":"option-disabled","label":"Pro","notice":"PUBLIC weekly limit"});
            }
            if matches!(self.fault, Fault::DisabledExtraHigh) {
                return json!({"status":"option-disabled","label":"Extra High","notice":"PUBLIC disabled row"});
            }
            if matches!(self.fault, Fault::GenericUnavailable) {
                return json!({"status":"option-not-found"});
            }
            if cfg["level"] == "pro" {
                return json!({"status":"switched","label":"Pro"});
            }
            return json!({"status":"switched","label":"Extra High","modelKind":"thinking"});
        }
        if expression.contains("industrious-dom:temporary-chat") {
            if let Some(state) = self.sequential_privacy() {
                return state;
            }
            return json!(
                !(matches!(self.fault, Fault::ProviderRecoveryPrivacy)
                    && self.provider_recovery_started())
            );
        }
        if expression.contains("industrious-dom:navigation") {
            // A Work-mode borrowed tab stays in Work until Industrious navigates it.
            let work = matches!(self.fault, Fault::BorrowedWork)
                && !self
                    .trace
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r["method"] == "Page.navigate");
            return json!({"currentUrl":self.url,"workSelected":work,"modeUnverified":false});
        }
        if expression.contains("industrious-dom:prompt-prepare") {
            self.prompt = cfg["prompt"].as_str().unwrap().into();
            return json!({"ready":true,"blocked":null,"convertedToFile":false});
        }
        if expression.contains("guard.insert(cfg.prompt)") {
            return json!({"ready":true,"blocked":null,"convertedToFile":false,"sawInput":true});
        }
        if expression.contains("guard.fallback(cfg.prompt)") {
            if matches!(self.fault, Fault::BorrowedPartialPrompt) {
                return json!({"ready":true,"complete":false,"sawInput":true,"blocked":null,"convertedToFile":false,"length":10});
            }
            if cfg["prompt"]
                .as_str()
                .is_some_and(|prompt| prompt.len() >= 50_000)
                && matches!(
                    self.fault,
                    Fault::TruncatedComposer
                        | Fault::SmallComposerMismatch
                        | Fault::PromptCleanupFailed
                        | Fault::PromptRollbackFailed
                )
            {
                let length = cfg["prompt"].as_str().unwrap().encode_utf16().count();
                return json!({"ready":true,"complete":false,"sawInput":true,"blocked":null,"convertedToFile":false,"length":if matches!(self.fault,Fault::SmallComposerMismatch){length-1000}else{30_000}});
            }
            return json!({"ready":true,"complete":true,"sawInput":true,"blocked":null,"convertedToFile":false});
        }
        if expression.contains("?.seal?.()") {
            return json!({"ready":true,"complete":true,"blocked":null,"convertedToFile":false,"guardId":cfg["guardId"],"listenersDetached":true,"guardRetained":true});
        }
        if expression.contains("industrious-dom:prompt-check") {
            return json!({"ready":true,"complete":true,"blocked":null,"convertedToFile":false});
        }
        if expression.contains("industrious-dom:completion-install") {
            return json!({"baseline":0});
        }
        if expression.contains("industrious-dom:send-focus") {
            let calls = self
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|r| {
                    r["params"]["expression"]
                        .as_str()
                        .is_some_and(|s| s.contains("industrious-dom:send-focus"))
                })
                .count();
            return match self.fault {
                Fault::SendSettling if calls <= 2 => json!({"status":"unavailable"}),
                Fault::SendNeverReady | Fault::SendAttachmentsNeverReady => {
                    json!({"status":"unavailable"})
                }
                _ => json!({"status":"focused"}),
            };
        }
        if expression.contains("industrious-dom:chat-list-rate-limit") {
            return json!(matches!(self.fault, Fault::ChatListLimited));
        }
        if expression.contains("industrious-dom:send-prepare") {
            if matches!(self.fault, Fault::AttemptParentBeforeIntent) {
                let child = self.store.require(&self.id).unwrap();
                let parent = child["browser"]["attempt"]["parentId"].as_str().unwrap();
                self.store
                    .update(parent, |_| Ok(Some(json!({"status":"cancelled"}))))
                    .unwrap();
            }
            if matches!(self.fault, Fault::SequentialChangedPrior)
                && self.sends.load(Ordering::SeqCst) == 1
            {
                self.store.update(&self.id,|current|Ok(Some(json!({"dispatch":merge_object(&current["dispatch"],&json!({"promptSha256":"PUBLIC changed prior dispatch"}))})))).unwrap();
            }
            return json!({"guardInstalled":true,"focused":true,"attachmentsReady":true,"promptReady":true,"preparationReady":true,"contextMatches":true});
        }
        if expression.contains("const summary={cleaned:true,sawKeyDown:") {
            return json!({"cleaned":true,"sawKeyDown":true,"sawClick":true,"admitted":true,"blocked":null});
        }
        if expression.contains("guard.cleanup(); delete guards[cfg.guardId]") {
            return json!({"cleaned":!matches!(self.fault,Fault::PromptCleanupFailed)});
        }
        if expression.contains("industrious-dom:prompt-rollback") {
            return json!({"restored":!matches!(self.fault,Fault::PromptRollbackFailed)});
        }
        if expression.contains("industrious-dom:result") {
            if matches!(self.fault, Fault::RecoveryParentCancelledAtAnswer) {
                let child = self.store.require(&self.id).unwrap();
                self.store
                    .update(
                        child["browser"]["attempt"]["parentId"].as_str().unwrap(),
                        |_| Ok(Some(json!({"status":"cancelled"}))),
                    )
                    .unwrap();
            }
            if self.recovery_observed && matches!(self.fault, Fault::CancelOutcome) {
                self.cancel.cancel();
                self.fault = Fault::None;
            }
            if self.download_count > 0 {
                match self.fault {
                    Fault::TargetDuringDownload => {
                        self.store.update(&self.id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"chromeTargetId":"REPLACED-PUBLIC","ownedRecoveryTarget":merge_object(&current["browser"]["runtime"]["ownedRecoveryTarget"],&json!({"targetId":"REPLACED-PUBLIC"}))}))}))})))).unwrap();
                    }
                    Fault::ModelDuringDownload => {
                        let model = self.store.require(&self.id).unwrap()["model"]
                            .as_str()
                            .unwrap()
                            .to_owned();
                        self.store.update_model(&self.id,&model,&json!({"status":"partial","marker":"PUBLIC winning model outcome"})).unwrap();
                    }
                    Fault::PartialDuringDownload | Fault::CancelledDuringDownload => {
                        self.store.update(&self.id,|_|Ok(Some(json!({"status":if matches!(self.fault,Fault::PartialDuringDownload){"partial"}else{"cancelled"},"completedAt":"PUBLIC winning terminal time","marker":"PUBLIC winning session outcome"})))).unwrap();
                    }
                    Fault::OptionsDuringDownload => {
                        self.store.update(&self.id,|current|Ok(Some(json!({"options":merge_object(&current["options"],&json!({"writeOutputPath":std::path::Path::new(current["cwd"].as_str().unwrap()).join("PUBLIC-foreign-output.md")}))})))).unwrap();
                    }
                    Fault::CancelDuringDownload => self.cancel.cancel(),
                    _ => {}
                }
            }
            self.result_reads += 1;
            let mut result = self.result();
            match self.fault {
                Fault::ContentWithheld => {
                    result["text"] = json!("This content can't be shown");
                    result["withheld"] = json!(true);
                    result["unloaded"] = json!(false);
                }
                Fault::ResponseLoadFailedBesideImage => {
                    result["text"] = json!("This response couldn\u{2019}t load");
                    result["images"] = json!([{"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public","alt":"Generated image","width":12,"height":12}]);
                }
                Fault::RetryOffered if cfg["uiErrors"] == true => {
                    return json!({"uiError":"temporary_unavailable"});
                }
                _ => {}
            }
            let recovering = self.provider_recovery_started();
            if matches!(
                self.fault,
                Fault::ProviderLateCommitPendingAnswer | Fault::ProviderLateCommitChangedOwner
            ) || matches!(self.fault, Fault::ProviderLateAnswerDeadline) && !recovering
            {
                result["completionVisible"] = json!(false);
            }
            if !recovering
                && matches!(
                    self.fault,
                    Fault::ProviderLateAnswer
                        | Fault::ProviderRecoveryWrongSource
                        | Fault::ProviderRecoveryChangedRequest
                        | Fault::ProviderRecoveryChangedOwner
                        | Fault::ProviderRecoveryCancelled
                        | Fault::ProviderRecoveryPrivacy
                )
            {
                result["text"] = json!("This response couldn’t load");
            }
            if recovering {
                match self.fault {
                    Fault::ProviderRecoveryWrongSource => {
                        result["lastUserOriginalSourceText"] =
                            json!("PUBLIC changed recovery source")
                    }
                    Fault::ProviderRecoveryChangedOwner | Fault::ProviderLateCommitChangedOwner => {
                        self.store.update(&self.id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"controllerPid":0}))}))})))).unwrap();
                    }
                    Fault::ProviderRecoveryChangedRequest => {
                        let mut request = request(&self.store, &self.id).unwrap();
                        request.prompt = "PUBLIC changed committed recovery request".into();
                        request.prompt_sha256 = sha256(request.prompt.as_bytes());
                        atomic_json(
                            &self
                                .store
                                .directory(&self.id)
                                .unwrap()
                                .join("browser-request.json"),
                            &request,
                        )
                        .unwrap();
                    }
                    Fault::ProviderRecoveryCancelled => self.cancel.cancel(),
                    _ => {}
                }
            }
            if self.pending_retirement_read
                && matches!(self.fault, Fault::CloseRefusedChangedSource)
            {
                result["lastUserOriginalSourceText"] = json!("PUBLIC changed pending source");
            }
            if self.pending_retirement_read
                && matches!(self.fault, Fault::CloseRefusedActiveThinking)
            {
                result["strongThinkingActive"] = json!(true);
            }
            if self.pending_retirement_read
                && matches!(self.fault, Fault::CloseRefusedMissingCompletion)
            {
                result["completionVisible"] = json!(false);
            }
            if matches!(self.fault, Fault::TwoFilePartial)
                && self.download_count == 2
                && !self.recovery_observed
            {
                result["lastAssistantTurnId"] = json!("PUBLIC interrupted second file publication");
            }
            if cfg["retirementId"].is_string() {
                if matches!(self.fault, Fault::AnswerDuringRetirement)
                    || self.recovery_observed
                        && matches!(self.fault, Fault::RecoveryAnswerDuringRetirement)
                {
                    result["text"] = json!("PUBLIC changed completed answer");
                }
                if matches!(self.fault, Fault::SourceDuringRetirement)
                    || self.recovery_observed
                        && matches!(self.fault, Fault::RecoverySourceDuringRetirement)
                {
                    result["lastUserOriginalSourceText"] = json!("PUBLIC changed original source");
                }
            }
            if matches!(
                self.fault,
                Fault::DelayedImage | Fault::NonImageThenImage | Fault::DelayedBadImage
            ) && self.result_reads >= 6
            {
                result["images"] = json!([{"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public","alt":"Generated image","width":12,"height":12}]);
            }
            if matches!(
                self.fault,
                Fault::MultiImage | Fault::PartialImage | Fault::FirstBadMultiImage
            ) {
                result["images"].as_array_mut().unwrap().push(json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public_2","alt":"Generated image 2","width":12,"height":12}));
            }
            if matches!(self.fault, Fault::DuplicateImage) {
                result["images"].as_array_mut().unwrap().push(json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public&preview=large","alt":"Generated larger image","width":24,"height":24}));
            }
            return result;
        }
        if expression.contains("industrious-dom:recovery") {
            self.recovery_observed = true;
            if matches!(self.fault, Fault::OrphanModelError) {
                let model = self.store.require(&self.id).unwrap()["model"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                self.store
                    .update_model(
                        &self.id,
                        &model,
                        &json!({"status":"error","marker":"PUBLIC concurrent terminal model"}),
                    )
                    .unwrap();
            }
            return json!(if matches!(self.fault, Fault::CleanupCleanRetry) {
                "clean"
            } else {
                "submission"
            });
        }
        if expression.contains("industrious-dom:cancelled-settle") {
            return json!(matches!(self.fault, Fault::CancelSendGenerating));
        }
        if expression.contains("industrious-dom:retire-cancelled-quarantine") {
            assert_eq!(cfg["owner"], self.owner);
            assert_eq!(cfg["conversationId"], "consult-public");
            return json!(!matches!(self.fault, Fault::CancelSendCloseRefused));
        }
        if expression.contains("industrious-dom:retirement-rollback") {
            self.retirement_marker = None;
            return Value::Null;
        }
        if expression.contains("industrious-dom:completed-disposition-retire") {
            assert_eq!(cfg["owner"], self.owner);
            let saved = self.store.require(&self.id).unwrap();
            assert_eq!(saved["status"], "completed");
            assert_eq!(saved["dispatch"]["state"], "completed");
            let creator = &saved["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
            let journal = TargetJournal::open(&self.store).unwrap();
            let record = journal.read(&creator["allocationId"]).unwrap();
            journal.verify(&record).unwrap();
            assert_eq!(record["retirement"]["id"], cfg["retirementId"]);
            self.retirement_marker = Some(cfg["retirementId"].clone());
            return json!(true);
        }
        if expression.contains("industrious-dom:disposition-retire") {
            assert_eq!(self.retirement_marker.as_ref(), Some(&cfg["retirementId"]));
            return json!(true);
        }
        if expression.contains("industrious-dom:retire") {
            let accepted = !matches!(self.fault, Fault::ForeignRetirement)
                && (!matches!(
                    self.fault,
                    Fault::CleanupRetry | Fault::SequentialNamedDelayed
                ) || expression.contains("industrious-dom:retire-quarantine"))
                && (!matches!(
                    self.fault,
                    Fault::CleanupCleanRetry
                        | Fault::RecoveryAnswerDuringRetirement
                        | Fault::RecoverySourceDuringRetirement
                ) || self.recovery_observed);
            if accepted {
                self.retirement_marker = Some(cfg["retirementId"].clone());
            }
            return json!(accepted);
        }
        if expression.contains("industrious-dom:attachment-evidence") {
            return json!(true);
        }
        if expression.contains("industrious-dom:upload-reset") {
            return json!({"complete":true,"removed":cfg["uploads"].as_array().unwrap().len(),"originalInputsCleared":true});
        }
        if expression.contains("industrious-dom:file-input-prepare") {
            if matches!(self.fault, Fault::BorrowedUploadAckFailed) {
                return Value::Null;
            }
            let prepares = self
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|r| {
                    r["params"]["expression"]
                        .as_str()
                        .is_some_and(|e| e.contains("industrious-dom:file-input-prepare"))
                })
                .count();
            if matches!(
                self.fault,
                Fault::BorrowedSecondUploadPrepareFailed | Fault::SecondUploadFailed
            ) && prepares > 1
            {
                return json!({"ready":false});
            }
            return json!({"ready":true,"blocked":null});
        }
        if expression.contains("industrious-dom:web-search")
            && expression.contains("const candidates = Array.from(context.form.querySelectorAll")
        {
            // A refused file-input preparation leaves no composer '+' control to open.
            return json!({"status":"missing","startUrl":self.url});
        }
        if expression.contains("industrious-dom:file-input-seal") {
            return json!(true);
        }
        if expression.contains("return record.input;") {
            return json!("input-object");
        }
        if expression.contains("industrious-dom:data-transfer-chunk") {
            return json!(cfg["index"].as_u64().unwrap() + 1);
        }
        if expression.contains("industrious-dom:data-transfer-assign") {
            return json!({"success":true,"fileName":cfg["name"],"size":cfg["fileBytes"]});
        }
        if expression.contains("industrious-dom:attachment-ready") {
            return json!(!matches!(
                self.fault,
                Fault::UploadFailed | Fault::UploadCleanupFailed
            ));
        }
        if expression.contains("rollbackComplete:record.guard.rollbackComplete") {
            let mut receipt = json!({"id":cfg["id"],"owner":cfg["owner"],"navigation":cfg["navigation"],"abandon":cfg["abandon"],"cleaned":!matches!(self.fault,Fault::UploadCleanupFailed),"blocked":null,"rollbackComplete":true,"evidenceCleared":true});
            match self.fault {
                Fault::UploadCleanupWrongId => receipt["id"] = json!("PUBLIC-other-upload"),
                Fault::UploadCleanupWrongOwner => {
                    receipt["owner"]["reservationId"] = json!("PUBLIC-other-reservation")
                }
                Fault::UploadCleanupWrongNavigation => {
                    receipt["navigation"]["url"] = json!("https://chatgpt.com/c/PUBLIC-other")
                }
                _ => {}
            }
            return receipt;
        }
        panic!(
            "Unaccounted DOM operation: {}",
            expression.chars().take(100).collect::<String>()
        );
    }
    async fn run<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut self,
        mut socket: WebSocketStream<S>,
    ) {
        while let Some(Ok(Message::Text(frame))) = socket.next().await {
            let request: Value = serde_json::from_str(&frame).unwrap();
            self.trace.lock().unwrap().push(request.clone());
            if matches!(self.fault, Fault::ProviderLateCommitLostAck)
                && request["method"] == "Runtime.evaluate"
                && request["params"]["expression"]
                    .as_str()
                    .is_some_and(|expression| {
                        expression.contains("industrious-dom:submission")
                            && expression.contains("\"action\":\"acknowledge\"")
                    })
            {
                continue;
            }
            if matches!(self.fault, Fault::UploadObjectUnacknowledged)
                && request["method"] == "Runtime.evaluate"
                && request["params"]["returnByValue"] == false
                || matches!(self.fault, Fault::UploadDescribeUnacknowledged)
                    && request["method"] == "DOM.describeNode"
                || matches!(self.fault, Fault::UploadCommandUnacknowledged)
                    && request["method"] == "DOM.setFileInputFiles"
            {
                continue;
            }
            if request["method"] == "Runtime.evaluate"
                && request["params"]["expression"]
                    .as_str()
                    .is_some_and(|expression| self.guard_refuses(expression))
            {
                let thrown = json!({"id":request["id"],"result":{"result":{"type":"object","subtype":"error"},"exceptionDetails":{"text":"Uncaught"}}});
                if socket
                    .send(Message::Text(thrown.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            let result = match request["method"].as_str().unwrap() {
                "Network.setCookie" => {
                    assert_eq!(self.url, "about:blank");
                    assert!(matches!(
                        self.fault,
                        Fault::CookieBootstrap
                            | Fault::CookieRejected
                            | Fault::CookieCancelled
                            | Fault::CookieJournalFailure
                    ));
                    if matches!(self.fault, Fault::CookieCancelled) {
                        self.cancel.cancel();
                    }
                    json!({"success":matches!(self.fault,Fault::CookieBootstrap)})
                }
                "Target.createTarget" => {
                    assert!(!self.borrowed(), "A borrowed tab run created a target");
                    assert_eq!(request["params"]["url"], "about:blank");
                    json!({"targetId":"TARGET-PUBLIC"})
                }
                "Target.attachToTarget" => {
                    assert_eq!(request["params"]["flatten"], true);
                    json!({"sessionId":"SESSION-PUBLIC"})
                }
                "Target.getTargetInfo" => {
                    json!({"targetInfo":{"targetId":if matches!(self.fault,Fault::InitialTargetInspection){"PUBLIC foreign target"}else{"TARGET-PUBLIC"},"type":"page","url":self.url}})
                }
                "Page.navigate" => {
                    self.url = request["params"]["url"].as_str().unwrap().into();
                    json!({"frameId":"FRAME-PUBLIC"})
                }
                "Page.enable" | "Input.dispatchMouseEvent" => json!({}),
                "Emulation.setFocusEmulationEnabled" => {
                    assert_eq!(request["params"], json!({"enabled":true}));
                    json!({})
                }
                "Page.getFrameTree" => {
                    json!({"frameTree":{"frame":{"id":"FRAME-PUBLIC","url":self.url}}})
                }
                "Browser.setDownloadBehavior" => {
                    if request["params"]["behavior"] == "default"
                        && matches!(
                            self.fault,
                            Fault::FileExportChangedSource | Fault::FileExportFormatted
                        )
                        && !self.recovery_observed
                    {
                        let current = self.store.require(&self.id).unwrap();
                        let file = current["artifacts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|artifact| artifact["kind"] == "file")
                            .unwrap();
                        fs::write(
                            file["path"].as_str().unwrap(),
                            b"PUBLIC changed captured file bytes",
                        )
                        .unwrap();
                    }
                    if request["params"]["behavior"] == "allowAndName" {
                        let path = std::path::PathBuf::from(
                            request["params"]["downloadPath"].as_str().unwrap(),
                        );
                        assert!(path.starts_with(
                            self.store.directory(&self.id).unwrap().join("artifacts")
                        ));
                        self.download_stage = Some(path);
                    }
                    if request["params"]["behavior"] == "default"
                        && matches!(self.fault, Fault::NewerReceipt)
                    {
                        self.store.update(&self.id,|current|{
                            let mut artifacts=current["artifacts"].clone();
                            artifacts[0]["extension"]=json!("PUBLIC newer receipt");
                            Ok(Some(json!({"artifacts":artifacts,"browser":merge_object(&current["browser"],&json!({"harvest":{"marker":"PUBLIC newer harvest"}})),"dispatch":merge_object(&current["dispatch"],&json!({"marker":"PUBLIC newer dispatch"}))})))
                        }).unwrap();
                    }
                    json!({})
                }
                "Browser.cancelDownload" => json!({}),
                "Runtime.evaluate" => {
                    let expression = request["params"]["expression"].as_str().unwrap();
                    let value = self.evaluate(expression);
                    if expression.contains("industrious-dom:download-click") {
                        let guid = if self.download_count == 0 {
                            "33333333-3333-4333-8333-333333333333"
                        } else if self.download_count == 1 {
                            "33333333-3333-4333-8333-333333333334"
                        } else {
                            "33333333-3333-4333-8333-333333333335"
                        };
                        self.download_count += 1;
                        let bytes: &[u8] = if matches!(
                            self.fault,
                            Fault::Image
                                | Fault::FormattedImage
                                | Fault::SequentialImage
                                | Fault::SequentialNamedAll
                                | Fault::SequentialNamedFirst
                                | Fault::SequentialNamedLast
                                | Fault::SequentialNamedButtons
                                | Fault::SequentialNamedDelayed
                                | Fault::ImageOnly
                                | Fault::NamedImage
                                | Fault::OutputImage
                                | Fault::MultiImage
                                | Fault::DuplicateImage
                                | Fault::ExistingImage
                                | Fault::DelayedImage
                                | Fault::ButtonImage
                                | Fault::LateButtonImage
                                | Fault::MultiButtonImage
                                | Fault::MultiButtonLostImage
                                | Fault::ButtonOutputFailure
                                | Fault::TargetDuringDownload
                                | Fault::ModelDuringDownload
                                | Fault::PartialDuringDownload
                                | Fault::CancelledDuringDownload
                                | Fault::OptionsDuringDownload
                                | Fault::CancelDuringDownload
                                | Fault::NewerReceipt
                        ) || matches!(
                            self.fault,
                            Fault::SequentialNamedBad
                                | Fault::SequentialNamedBadButtons
                                | Fault::SequentialNamedFirstThenFile
                        ) && self.download_count == 1
                            || matches!(
                                self.fault,
                                Fault::NonImageThenImage
                                    | Fault::InvalidDirectButtonImage
                                    | Fault::FirstBadMultiImage
                            ) && self.download_count > 1
                            || matches!(self.fault, Fault::PartialImage) && self.download_count != 2
                        {
                            b"\x89PNG\r\n\x1a\nPUBLIC synthetic image bytes"
                        } else {
                            b"PUBLIC downloaded file bytes"
                        };
                        // Chrome on Windows fails the final rename into a download directory
                        // with forward slashes or a verbatim prefix and reports it as canceled.
                        let renamed =
                            !cfg!(windows)
                                || self.download_stage.as_ref().unwrap().to_str().is_some_and(
                                    |path| !path.contains('/') && !path.starts_with(r"\\?\"),
                                );
                        if renamed {
                            fs::write(self.download_stage.as_ref().unwrap().join(guid), bytes)
                                .unwrap();
                        }
                        let begin = json!({"method":"Browser.downloadWillBegin","params":{"frameId":if matches!(self.fault,Fault::ForeignDownloadFrame){"FOREIGN-FRAME"}else{"FRAME-PUBLIC"},"guid":guid,"suggestedFilename":if matches!(self.fault,Fault::Image|Fault::FormattedImage|Fault::BadImage|Fault::ImageOnly){"PUBLIC.png"}else{"PUBLIC.txt"}}});
                        if socket
                            .send(Message::Text(begin.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if matches!(self.fault, Fault::DuplicateDownloadGuid)
                            && socket
                                .send(Message::Text(begin.to_string().into()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                        let done = json!({"method":"Browser.downloadProgress","params":{"guid":guid,"state":if renamed {"completed"} else {"canceled"}}});
                        if matches!(self.fault, Fault::MultiButtonLostImage)
                            && self.download_count == 2
                        {
                            let response =
                                json!({"id":request["id"],"result":{"result":{"value":value}}});
                            if socket
                                .send(Message::Text(response.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        if socket
                            .send(Message::Text(done.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if matches!(self.fault, Fault::LostDownloadAck) {
                            break;
                        }
                    }
                    if request["params"]["returnByValue"] == false {
                        json!({"result":{"objectId":value}})
                    } else {
                        json!({"result":{"value":value}})
                    }
                }
                "DOM.describeNode" => json!({"node":{"backendNodeId":17}}),
                "DOM.setFileInputFiles" => json!({}),
                "Input.dispatchKeyEvent" => {
                    if request["params"]["type"] == "keyDown" {
                        let before = self.sends.fetch_add(1, Ordering::SeqCst);
                        assert!(
                            self.sequential() || before == 0,
                            "Send must never be replayed"
                        );
                        if self.sequential() {
                            self.turns.push(self.prompt.clone());
                        }
                        self.url = "https://chatgpt.com/c/consult-public".into();
                        if matches!(
                            self.fault,
                            Fault::CancelSend
                                | Fault::CancelSendGenerating
                                | Fault::CancelSendCloseRefused
                        ) {
                            self.cancel.cancel();
                        }
                        if before == 1 && matches!(self.fault, Fault::SequentialCancel) {
                            self.cancel.cancel();
                        }
                        if matches!(self.fault, Fault::ReplacedOwner) {
                            self.store.update(&self.id, |current| Ok(Some(json!({"startedAt":"replacement-controller","lifecycle":merge_object(&current["lifecycle"], &json!({"workerPid":424242}))})))).unwrap();
                        }
                        if matches!(self.fault, Fault::LostSendAck)
                            || (before == 1 && matches!(self.fault, Fault::SequentialLostAck))
                            || (before == 2 && matches!(self.fault, Fault::SequentialLastLostAck))
                        {
                            break;
                        }
                    }
                    json!({})
                }
                "Target.closeTarget" => {
                    assert!(!self.borrowed(), "A borrowed tab run closed the user's tab");
                    assert_eq!(request["params"]["targetId"], "TARGET-PUBLIC");
                    self.closes.fetch_add(1, Ordering::SeqCst);
                    self.closed = !matches!(
                        self.fault,
                        Fault::CloseRefusedOnce
                            | Fault::PreDispatchCloseRefused
                            | Fault::CloseRefusedThenLockedSettlement
                            | Fault::CloseRefusedTwice
                            | Fault::CloseRefusedForeignMarker
                            | Fault::CloseRefusedChangedSource
                            | Fault::CloseRefusedActiveThinking
                            | Fault::CloseRefusedMissingCompletion
                    ) || self.closes.load(Ordering::SeqCst)
                        > if matches!(self.fault, Fault::CloseRefusedTwice) {
                            2
                        } else {
                            1
                        };
                    if matches!(self.fault, Fault::CloseRefusedThenLockedSettlement)
                        && self.closes.load(Ordering::SeqCst) == 2
                    {
                        self.cleanup_lock = Some(
                            DirectoryLock::acquire(
                                &self
                                    .store
                                    .home()
                                    .join("sessions-write-locks")
                                    .join(format!("{}.lock", self.id)),
                                Duration::from_secs(1),
                            )
                            .unwrap(),
                        );
                    }
                    if matches!(self.fault, Fault::CookieJournalFailure) {
                        let meta = self.store.require(&self.id).unwrap();
                        let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
                        let journal = TargetJournal::open(&self.store).unwrap();
                        assert_eq!(
                            journal.read(&creator["allocationId"]).unwrap()["disposition"]["reason"],
                            "settled-pre-dispatch-close-pending"
                        );
                        let path = self.store.home().join("target-dispositions").join(format!(
                            "{}.json",
                            creator["allocationId"].as_str().unwrap()
                        ));
                        fs::rename(&path, path.with_extension("PUBLIC-pending")).unwrap();
                        fs::create_dir(&path).unwrap();
                    }
                    json!({"success":true})
                }
                "Network.getAllCookies" => {
                    if matches!(self.fault, Fault::StaleConversationCookie) {
                        json!({"cookies":[
                            {"name":"conv_key_PUBLIC-stale","value":"PUBLIC-cookie-value-sentinel","domain":".chatgpt.com","path":"/"},
                            {"name":"PUBLIC-session","value":"PUBLIC-cookie-value-sentinel","domain":".chatgpt.com","path":"/"}
                        ]})
                    } else {
                        json!({"cookies":[]})
                    }
                }
                "Network.deleteCookies" => {
                    assert!(matches!(self.fault, Fault::StaleConversationCookie));
                    assert_eq!(
                        request["params"],
                        json!({"name":"conv_key_PUBLIC-stale","domain":".chatgpt.com","path":"/"})
                    );
                    json!({})
                }
                "Target.getTargets" => {
                    if self.closed && matches!(self.fault, Fault::UncertainClose) {
                        break;
                    }
                    if self.borrowed() {
                        json!({"targetInfos":[{"targetId":"TARGET-PUBLIC","type":"page","url":self.url},{"targetId":"FOREIGN-PUBLIC","type":"page"}]})
                    } else {
                        json!({"targetInfos":if self.closed {vec![json!({"targetId":"FOREIGN-PUBLIC","type":"page"})]} else {vec![json!({"targetId":"TARGET-PUBLIC","type":"page"}),json!({"targetId":"FOREIGN-PUBLIC","type":"page"})]}})
                    }
                }
                other => panic!("Unaccounted CDP operation {other}"),
            };
            if socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    }
}
struct Fixture {
    home: tempfile::TempDir,
    store: SessionStore,
    id: String,
    identity: ProcessIdentity,
    profile: Arc<ProfileRegistry>,
    cdp: Cdp,
    peer: tokio::task::JoinHandle<()>,
    sends: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    trace: Arc<Mutex<Vec<Value>>>,
    cancel: CancellationToken,
    endpoint: AttachmentEndpoint,
    coordinator: Option<crate::browser_attempt::Parent>,
}
async fn fixture(fault: Fault, upload: bool) -> Fixture {
    fixture_attempt(fault, upload, false, json!({})).await
}
async fn fixture_attempt(fault: Fault, upload: bool, attempt: bool, options: Value) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    fixture_attempt_in(home, fault, upload, attempt, options).await
}
async fn fixture_attempt_in(
    home: tempfile::TempDir,
    fault: Fault,
    upload: bool,
    attempt: bool,
    options: Value,
) -> Fixture {
    let store = SessionStore::new(home.path().join(".oracle"));
    let mut input = json!({"prompt":"PUBLIC complete original consultation\n\nwith literal `source` and end.","engine":"browser"});
    if matches!(
        fault,
        Fault::CookieBootstrap
            | Fault::CookieRejected
            | Fault::CookieCancelled
            | Fault::CookieJournalFailure
    ) {
        input["browserInlineCookies"] =
            json!(r#"[{"name":"PUBLIC-cookie","value":"PUBLIC-cookie-sentinel"}]"#);
    }
    if upload {
        fs::write(home.path().join("public.txt"), b"PUBLIC file contents").unwrap();
        input["files"] = json!(["public.txt"]);
        input["browserAttachments"] = json!("always");
    }
    if matches!(
        fault,
        Fault::BorrowedSecondUploadPrepareFailed | Fault::SecondUploadFailed
    ) {
        fs::write(
            home.path().join("public-second.pdf"),
            b"%PDF-1.7 PUBLIC synthetic attachment",
        )
        .unwrap();
        input["files"] = json!(["public.txt", "public-second.pdf"]);
    }
    if matches!(
        fault,
        Fault::TruncatedComposer
            | Fault::SmallComposerMismatch
            | Fault::PromptCleanupFailed
            | Fault::PromptRollbackFailed
    ) {
        // Files always upload, so only a long prompt reaches the composer checks.
        input["prompt"] = json!(format!("PUBLIC {}", "x".repeat(52_000)));
    }
    if matches!(
        fault,
        Fault::NamedImage
            | Fault::MultiImage
            | Fault::DuplicateImage
            | Fault::PartialImage
            | Fault::FirstBadMultiImage
            | Fault::DelayedBadImage
            | Fault::ButtonOutputFailure
            | Fault::InvalidDirectButtonImage
            | Fault::ExistingImage
            | Fault::MissingImage
            | Fault::DelayedImage
            | Fault::ButtonImage
            | Fault::LateButtonImage
            | Fault::MultiButtonImage
            | Fault::MultiButtonLostImage
            | Fault::NonImageButton
            | Fault::NonImageThenImage
            | Fault::TargetDuringDownload
            | Fault::ModelDuringDownload
            | Fault::PartialDuringDownload
            | Fault::CancelledDuringDownload
            | Fault::OptionsDuringDownload
            | Fault::CancelDuringDownload
            | Fault::NewerReceipt
            | Fault::SequentialNamedAll
            | Fault::SequentialNamedFirst
            | Fault::SequentialNamedLast
            | Fault::SequentialNamedButtons
            | Fault::SequentialNamedBad
            | Fault::SequentialNamedBadButtons
            | Fault::SequentialNamedDelayed
            | Fault::SequentialNamedFirstThenFile
            | Fault::ImageUiWarning
    ) {
        input["generateImage"] = json!(home.path().join("PUBLIC-image.png"));
    }
    if matches!(
        fault,
        Fault::TargetDuringDownload
            | Fault::ModelDuringDownload
            | Fault::PartialDuringDownload
            | Fault::CancelledDuringDownload
            | Fault::OptionsDuringDownload
            | Fault::CancelDuringDownload
            | Fault::CleanupRetry
            | Fault::CleanupCleanRetry
            | Fault::AnswerDuringRetirement
            | Fault::SourceDuringRetirement
            | Fault::RecoveryAnswerDuringRetirement
            | Fault::RecoverySourceDuringRetirement
            | Fault::ForeignRetirement
    ) {
        input["writeOutputPath"] = json!(home.path().join("PUBLIC-output.md"));
    }
    if matches!(fault, Fault::OutputImage) {
        input["outputPath"] = json!(home.path().join("PUBLIC-image.png"));
    }
    if matches!(fault, Fault::ExistingImage) {
        fs::write(
            home.path().join("PUBLIC-image.png"),
            b"PUBLIC existing image destination",
        )
        .unwrap();
    }
    if matches!(
        fault,
        Fault::FileExport
            | Fault::FileExportNoAnswer
            | Fault::FileExportChangedSource
            | Fault::FileExportFormatted
    ) {
        input["writeArtifacts"] = json!(true);
        if matches!(
            fault,
            Fault::FileExport | Fault::FileExportChangedSource | Fault::FileExportFormatted
        ) {
            input["writeOutputPath"] = json!(home.path().join("PUBLIC-output.md"));
        }
    }
    if matches!(fault, Fault::TwoFilePartial) {
        input["writeArtifacts"] = json!(true);
        input["writeOutputPath"] = json!(home.path().join("PUBLIC-output.md"));
    }
    if matches!(fault, Fault::ButtonOutputFailure) {
        input["writeOutputPath"] = json!(home.path().join("PUBLIC-output.md"));
        fs::create_dir(home.path().join("PUBLIC-output.md")).unwrap();
        fs::write(
            home.path().join("PUBLIC-output.md/PUBLIC-sentinel.txt"),
            b"PUBLIC preserved output",
        )
        .unwrap();
    }
    input = merge_object(&input, &options);
    let config = json!({});
    let mut resolved =
        crate::execution::resolve(&input, &config, &Environment::new(), home.path()).unwrap();
    if matches!(
        fault,
        Fault::Sequential
            | Fault::SequentialExtraHigh
            | Fault::SequentialProModel
            | Fault::SequentialExtraHighProModel
            | Fault::SequentialWrongModel
            | Fault::SequentialWrongTier
            | Fault::SequentialChangedPrior
            | Fault::SequentialLostAck
            | Fault::SequentialCancel
            | Fault::SequentialTimeout
            | Fault::SequentialFile
            | Fault::SequentialImage
            | Fault::SequentialLastLostAck
            | Fault::SequentialNamedAll
            | Fault::SequentialNamedFirst
            | Fault::SequentialNamedLast
            | Fault::SequentialNamedButtons
            | Fault::SequentialNamedBad
            | Fault::SequentialNamedBadButtons
            | Fault::SequentialNamedDelayed
            | Fault::SequentialNamedFirstThenFile
            | Fault::SequentialPrivacySettling
            | Fault::SequentialPrivacyUnknown
            | Fault::SequentialPrivacyOff
    ) {
        resolved.options["browserFollowUps"] =
            json!([" PUBLIC second turn ", "  ", "PUBLIC third turn"]);
        resolved.options["writeOutputPath"] = json!(home.path().join("PUBLIC-sequence.md"));
    }
    if matches!(
        fault,
        Fault::CancelPickerKeep | Fault::CancelPickerKeepClose
    ) {
        resolved.browser["keepBrowser"] = json!(true);
    }
    if matches!(fault, Fault::CancelPickerKeepClose) {
        resolved.browser["closeOwnedTabOnCancel"] = json!(true);
    }
    if matches!(fault, Fault::SendAttachmentsNeverReady) {
        resolved.browser["attachmentTimeoutMs"] = json!(1000);
        resolved.options["browserConfig"] = resolved.browser.clone();
    }
    if matches!(fault, Fault::CancelPickerRefuse) {
        resolved.browser["closeOwnedTabOnCancel"] = json!(false);
    }
    if matches!(
        fault,
        Fault::SignedOut | Fault::ManualLoginLate | Fault::ManualLoginTimeout
    ) {
        resolved.browser["manualLogin"] = json!(!matches!(fault, Fault::SignedOut));
        if matches!(fault, Fault::ManualLoginTimeout) {
            resolved.browser["timeoutMs"] = json!(1500);
        }
        resolved.options["browserConfig"] = resolved.browser.clone();
    }
    let borrowed = matches!(
        fault,
        Fault::Borrowed
            | Fault::BorrowedWork
            | Fault::BorrowedWrongTier
            | Fault::BorrowedClaimed
            | Fault::BorrowedPartialPrompt
            | Fault::BorrowedSecondUploadPrepareFailed
            | Fault::BorrowedUploadAckFailed
            | Fault::BorrowedReleaseFailed
    );
    if borrowed {
        resolved.browser["browserTabRef"] = json!("TARGET-PUBLIC");
        resolved.options["browserConfig"] = resolved.browser.clone();
    }
    if matches!(
        fault,
        Fault::CancelPickerDefault
            | Fault::CancelPickerKeep
            | Fault::CancelPickerKeepClose
            | Fault::CancelPickerRefuse
    ) {
        resolved.options["browserConfig"] = resolved.browser.clone();
    }
    if matches!(
        fault,
        Fault::Followup
            | Fault::LegacyFollowup
            | Fault::FollowupExtraHigh
            | Fault::FollowupWrongModel
            | Fault::FollowupWrongTier
    ) {
        if matches!(fault, Fault::FollowupExtraHigh) {
            resolved.browser["thinkingTime"] = json!("extra-high");
        }
        let url = if matches!(fault, Fault::LegacyFollowup) {
            "https://chat.openai.com/c/consult-public"
        } else {
            "https://chatgpt.com/c/consult-public"
        };
        let parent = store
            .create(
                json!({"prompt":"PUBLIC parent consultation","model":"gpt-6-pro","mode":"browser"}),
                home.path(),
                Some("public-parent"),
            )
            .unwrap();
        let parent_id = parent["id"].as_str().unwrap();
        store.update(parent_id,|_|Ok(Some(json!({"status":"completed","browser":{"config":resolved.browser,"runtime":{"tabUrl":url,"chromeTargetId":"PARENT-TARGET","ownedRecoveryTarget":{"targetId":"PARENT-TARGET"}}}})))).unwrap();
        match crate::followup::resolve(&store, parent_id, None).unwrap() {
            crate::followup::Followup::Browser {
                session_id,
                model,
                config,
            } => {
                resolved.options["followupSessionId"] = json!(session_id);
                resolved.options["model"] = json!(model);
                resolved.options["browserResumeConversationUrl"] =
                    config["resumeConversationUrl"].clone();
                resolved.options["browserConfig"] = config.clone();
                resolved.browser = config;
            }
            _ => panic!("browser parent resolved as API"),
        }
    }
    let meta = if matches!(fault, Fault::RemoteCapture | Fault::RemoteCaptureRejected) {
        let context = crate::remote_host::RunContext::fixture(
            "12345678-1234-1234-1234-123456789abc",
            json!({"prompt":resolved.options["prompt"],"options":{"model":resolved.options["model"]},"browserConfig":resolved.browser}),
            store.clone(),
        );
        let driver = crate::remote_driver::BrowserDriver {
            environment: Environment::new(),
            cwd: home.path().into(),
            browser_config: resolved.browser.clone(),
        };
        let id = driver.prepare(&context).unwrap();
        store.require(&id).unwrap()
    } else if matches!(fault, Fault::SequentialNamedDelayed) {
        prepare_with_base(
            &store,
            &resolved,
            home.path(),
            Some(
                &crate::remote_result::session_id("12345678-1234-1234-1234-123456789abc").unwrap(),
            ),
        )
        .unwrap()
    } else {
        // Below the image admission refusal, so the image delivery fixtures still run.
        prepare_with_base(&store, &resolved, home.path(), None).unwrap()
    };
    let mut id = meta["id"].as_str().unwrap().to_owned();
    let identity = ProcessIdentity::current().unwrap();
    start_owner(&store, &id, &identity).unwrap();
    let mut coordinator = None;
    if attempt
        || matches!(
            fault,
            Fault::AttemptParentBeforeIntent | Fault::AttemptParentBeforeSend
        )
    {
        let mut parent = crate::browser_attempt::Parent::begin(
            &store,
            &id,
            &identity,
            &CancellationToken::new(),
        )
        .unwrap();
        id = parent
            .reserve_first(&CancellationToken::new())
            .unwrap()
            .child_id;
        start_owner(&store, &id, &identity).unwrap();
        coordinator = Some(parent);
    }
    if coordinator.is_none()
        && matches!(
            fault,
            Fault::RemoteCapture | Fault::RemoteCaptureRejected | Fault::SequentialNamedDelayed
        )
    {
        store.update(&id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"remoteDelivery":{"runId":"12345678-1234-1234-1234-123456789abc","acknowledged":false,"producerFinished":false}}))})))).unwrap();
    }
    let profile = ProfileRegistry::open(&home.path().join("synthetic-profile")).unwrap();
    let (client, server) = tokio::io::duplex(1024 * 1024);
    let cdp = Cdp::from_socket(
        WebSocketStream::from_raw_socket(client, Role::Client, None).await,
        Duration::from_secs(1),
    );
    let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let sends = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let trace = Arc::new(Mutex::new(vec![]));
    let cancel = CancellationToken::new();
    let peer = tokio::spawn(
        Exchange {
            fault,
            prompt: String::new(),
            url: if borrowed {
                "https://chatgpt.com/"
            } else {
                "about:blank"
            }
            .into(),
            owner: Value::Null,
            commit: Value::Null,
            before: true,
            turns: vec![],
            sends: sends.clone(),
            closes: closes.clone(),
            closed: false,
            trace: trace.clone(),
            cancel: cancel.clone(),
            store: store.clone(),
            id: id.clone(),
            download_stage: None,
            result_reads: 0,
            recovery_observed: false,
            download_count: 0,
            delayed_image_visible: false,
            retirement_marker: None,
            pending_retirement_read: false,
            cleanup_lock: None,
        }
        .run(server),
    );
    Fixture {
        home,
        store,
        id,
        identity,
        profile,
        cdp,
        peer,
        sends,
        closes,
        trace,
        cancel,
        endpoint: AttachmentEndpoint {
            host: "127.0.0.1".into(),
            port: 9222,
            browser_ws_endpoint: "ws://127.0.0.1:9222/devtools/browser/PUBLIC".into(),
        },
        coordinator,
    }
}
fn limits() -> RunnerLimits {
    RunnerLimits {
        // The command budget also bounds steps that publish several native records, such as
        // target allocation. Under a parallel suite one flush can stall for over a second, so
        // one second is too tight; lost-acknowledgement cases wait out this budget instead.
        command: Duration::from_secs(5),
        // These protocol fixtures test browser outcomes, not admission speed.
        // Leave room for native identity/publication I/O under parallel Windows tests.
        // Dedicated admission deadline cases supply their own short budget below.
        admission: Duration::from_secs(5),
        profile_lock: Duration::from_secs(5),
        commit: Duration::from_millis(500),
        answer: Duration::from_secs(1),
        poll: Duration::from_millis(1),
        stable_ms: 2,
        confirm_cycles: 3,
        bringup: Duration::from_secs(5),
    }
}
/// Limits for cases whose commit or answer deadline expires before a bounded provider read.
/// That read's whole budget is min(commit, answer), so the fixture's 500 ms leaves too little
/// for its identity checks and receipts once a parallel suite stalls native publication.
fn recovery_limits() -> RunnerLimits {
    RunnerLimits {
        commit: Duration::from_secs(3),
        answer: Duration::from_secs(3),
        ..limits()
    }
}
/// The original one-second command budget, for cases whose log text names it or whose
/// elapsed bound is derived from it.
fn one_second_command_limits() -> RunnerLimits {
    RunnerLimits {
        command: Duration::from_secs(1),
        ..limits()
    }
}
async fn run(f: &Fixture) -> Result<Value> {
    run_within(f, &limits()).await
}
async fn run_within(f: &Fixture, limits: &RunnerLimits) -> Result<Value> {
    execute(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &f.identity,
        limits,
        &f.cancel,
    )
    .await
}
async fn stop(f: Fixture) {
    f.cdp
        .disconnect_and_wait(Duration::from_secs(1))
        .await
        .unwrap();
    f.peer.await.unwrap();
}
async fn run_attempt_capture(f: &Fixture) -> Result<Value> {
    run_attempt_capture_within(f, &limits()).await
}
async fn run_attempt_capture_within(f: &Fixture, limits: &RunnerLimits) -> Result<Value> {
    execute_with_boundary(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &f.identity,
        limits,
        &f.cancel,
        &CaptureBoundary::ParentAttempt,
        None,
        None,
        None,
    )
    .await
}
#[tokio::test]
#[ignore = "Subprocess fixture, invoked only by public-parent recovery contracts"]
async fn orphan_provider_attempt_helper() {
    let Some(root) = std::env::var_os("INDUSTRIOUS_SYNTHETIC_ORPHAN") else {
        return;
    };
    let root = PathBuf::from(root);
    let sequential = std::env::var("INDUSTRIOUS_SYNTHETIC_SEQUENCE").as_deref() == Ok("1");
    let completed = std::env::var("INDUSTRIOUS_SYNTHETIC_PARENT_COMPLETED").as_deref() == Ok("1");
    let capture_kind = std::env::var("INDUSTRIOUS_SYNTHETIC_CAPTURE_KIND").unwrap_or_default();
    let home = tempfile::Builder::new()
        .prefix("PUBLIC-orphan-")
        .tempdir_in(&root)
        .unwrap();
    let mut f = fixture_attempt_in(
        home,
        if capture_kind.starts_with("remote") {
            Fault::RemoteCapture
        } else if matches!(capture_kind.as_str(), "formatted" | "captured-formatted") {
            Fault::FormattedMarkdown
        } else if matches!(
            capture_kind.as_str(),
            "formatted-image" | "captured-image" | "reserved-image"
        ) {
            Fault::FormattedImage
        } else if matches!(
            capture_kind.as_str(),
            "captured-file" | "reserved-file" | "reserved-parent-file"
        ) {
            Fault::File
        } else if matches!(capture_kind.as_str(), "captured-export" | "reserved-export") {
            Fault::FileExport
        } else if capture_kind == "images" {
            Fault::SequentialNamedLast
        } else if capture_kind == "buttons" {
            Fault::SequentialNamedButtons
        } else if capture_kind == "sequence" {
            Fault::Sequential
        } else if completed || !capture_kind.is_empty() {
            Fault::None
        } else if sequential {
            Fault::SequentialLastLostAck
        } else {
            Fault::LostSendAck
        },
        false,
        true,
        if capture_kind.starts_with("remote") {
            json!({})
        } else {
            json!({"writeOutputPath":"PUBLIC-recovered.md"})
        },
    )
    .await;
    if let Ok(port) = std::env::var("INDUSTRIOUS_SYNTHETIC_ENDPOINT_PORT") {
        let port: u16 = port.parse().unwrap();
        f.endpoint.port = port;
        f.endpoint.browser_ws_endpoint = format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC");
    }
    let result = run_attempt_capture(&f).await;
    if completed || !capture_kind.is_empty() {
        result.unwrap();
        if capture_kind.starts_with("reserved") {
            let stop = if capture_kind == "reserved" {
                "reserved"
            } else if capture_kind == "reserved-parent-file" {
                "parent-file"
            } else {
                "artifact"
            };
            let error = f
                .coordinator
                .as_mut()
                .unwrap()
                .capture_until_for_test(&f.id, stop, &f.cancel)
                .unwrap_err();
            assert!(error.to_string().contains("PUBLIC interrupted"));
        } else if capture_kind.starts_with("captured") {
            f.coordinator
                .as_mut()
                .unwrap()
                .capture_only_for_test(&f.id, &f.cancel)
                .unwrap();
            let parent_id = f.store.require(&f.id).unwrap()["browser"]["attempt"]["parentId"]
                .as_str()
                .unwrap()
                .to_owned();
            if capture_kind == "captured-existing" {
                let parent = f.store.require(&parent_id).unwrap();
                let bytes = captured(&f.store, &parent_id).unwrap();
                crate::browser_output::save_answer(
                    &f.store,
                    &f.home.path().join("PUBLIC-recovered.md"),
                    &bytes,
                    &crate::browser_output::answer_heading(&parent),
                    &f.cancel,
                )
                .unwrap();
            } else if capture_kind == "captured-conflict" {
                fs::write(
                    f.home.path().join("PUBLIC-recovered.md"),
                    b"PUBLIC external answer",
                )
                .unwrap();
            }
        } else if capture_kind.starts_with("remote-") && capture_kind != "remote-selected" {
            let coordinator = f.coordinator.as_mut().unwrap();
            coordinator.adopt(&f.id, &f.cancel).unwrap();
            let store = f.store.clone();
            let missing = capture_kind == "remote-callback-missing";
            let hook: CaptureHook = Arc::new(move |captured| {
                anyhow::ensure!(
                    !missing,
                    "PUBLIC callback interrupted before capture record"
                );
                let run = "12345678-1234-1234-1234-123456789abc";
                let produced = crate::remote_protocol::produced(&store, run, captured, false)?;
                crate::remote_result::save(
                    &store,
                    run,
                    &produced.captured,
                    &produced.result,
                    &produced.artifacts,
                    crate::remote_result::Phase::Capture,
                    &CancellationToken::new(),
                )?;
                Ok(())
            });
            let issued = coordinator.issue_capture(&hook, &f.cancel);
            if missing {
                assert!(issued.is_err());
            } else {
                issued.unwrap();
            }
            let run = "12345678-1234-1234-1234-123456789abc";
            if matches!(
                capture_kind.as_str(),
                "remote-partial" | "remote-partial-result"
            ) {
                let record = crate::remote_result::read(&f.store, run).unwrap();
                let (parent, committed) =
                    crate::remote_result::complete_acknowledgement_lossless(&f.store, &record)
                        .unwrap();
                assert!(committed);
                assert_eq!(parent["status"], "partial");
            }
            if matches!(
                capture_kind.as_str(),
                "remote-result" | "remote-partial-result"
            ) {
                let record = crate::remote_result::read(&f.store, run).unwrap();
                crate::remote_result::promote_capture(&f.store, &record, &f.cancel, None).unwrap();
            }
        } else if completed || matches!(capture_kind.as_str(), "selected" | "remote-selected") {
            let coordinator = f.coordinator.as_mut().unwrap();
            coordinator.adopt(&f.id, &f.cancel).unwrap();
            if completed {
                coordinator.complete(&f.cancel).unwrap();
            }
        }
    } else {
        assert!(result.is_err());
    }
    assert_eq!(
        f.sends.load(Ordering::SeqCst),
        if sequential { 3 } else { 1 },
        "The fixture must use only its original physical Sends"
    );
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    let child = f.store.require(&f.id).unwrap();
    let manifest = json!({"home":f.home.path(),"parentId":child["browser"]["attempt"]["parentId"],
        "childId":f.id,"identity":f.identity,"sends":f.sends.load(Ordering::SeqCst)});
    f.cdp
        .disconnect_and_wait(Duration::from_secs(1))
        .await
        .unwrap();
    f.peer.await.unwrap();
    // Keep the original native directories and seals, inside the caller's temporary root.
    let _ = f.home.keep();
    crate::storage::atomic_json(&root.join("ready.json"), &manifest).unwrap();
}

pub(crate) fn dead_provider_attempt() -> (tempfile::TempDir, Value) {
    dead_provider_attempt_kind(false)
}
fn dead_provider_attempt_kind(sequential: bool) -> (tempfile::TempDir, Value) {
    dead_provider_attempt_state(sequential, false, None, None)
}
pub(crate) fn completed_parent_attempt() -> (tempfile::TempDir, Value) {
    dead_provider_attempt_state(false, true, None, None)
}
pub(crate) fn selected_parent_attempt() -> (tempfile::TempDir, Value) {
    dead_provider_attempt_state(false, false, None, Some("selected"))
}
pub(crate) fn remote_parent_attempt(kind: &str) -> (tempfile::TempDir, Value) {
    dead_provider_attempt_state(false, false, None, Some(kind))
}
pub(crate) fn reserved_parent_attempt() -> (tempfile::TempDir, Value) {
    dead_provider_attempt_state(false, false, None, Some("reserved"))
}
pub(crate) fn captured_parent_attempt() -> (tempfile::TempDir, Value) {
    dead_provider_attempt_state(false, false, None, Some("captured"))
}
fn dead_provider_attempt_state(
    sequential: bool,
    completed: bool,
    port: Option<u16>,
    capture_kind: Option<&str>,
) -> (tempfile::TempDir, Value) {
    struct Helper(std::process::Child);
    impl Drop for Helper {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let stdout = fs::File::create(root.path().join("helper.stdout")).unwrap();
    let stderr = fs::File::create(root.path().join("helper.stderr")).unwrap();
    let mut helper = Helper(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "browser_runner::tests::orphan_provider_attempt_helper",
                "--nocapture",
            ])
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("INDUSTRIOUS_SYNTHETIC_ORPHAN", root.path())
            .env(
                "INDUSTRIOUS_SYNTHETIC_SEQUENCE",
                if sequential { "1" } else { "0" },
            )
            .env(
                "INDUSTRIOUS_SYNTHETIC_PARENT_COMPLETED",
                if completed { "1" } else { "0" },
            )
            .env(
                "INDUSTRIOUS_SYNTHETIC_ENDPOINT_PORT",
                port.unwrap_or(9222).to_string(),
            )
            .env(
                "INDUSTRIOUS_SYNTHETIC_CAPTURE_KIND",
                capture_kind.unwrap_or(""),
            )
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = helper.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Synthetic provider owner did not exit before its deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "Synthetic provider owner failed: {} {}",
        fs::read_to_string(root.path().join("helper.stdout")).unwrap(),
        fs::read_to_string(root.path().join("helper.stderr")).unwrap()
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("ready.json")).unwrap()).unwrap();
    let identity: ProcessIdentity = serde_json::from_value(manifest["identity"].clone()).unwrap();
    assert_eq!(identity.pid, helper.0.id());
    assert_eq!(identity.liveness(), Liveness::Dead);
    (root, manifest)
}

#[tokio::test]
async fn public_parent_recovers_its_dead_original_attempt_without_replaying() {
    for sequential in [false, true] {
        assert_parent_recovery(sequential, None).await;
    }
}
#[tokio::test]
async fn public_parent_recovers_final_sequential_images_and_button_downloads() {
    for fault in [Fault::SequentialNamedLast, Fault::SequentialNamedButtons] {
        assert_parent_recovery(true, Some(fault)).await;
    }
}
#[tokio::test]
async fn public_parent_cancellation_stops_recovered_acknowledgement_and_capture() {
    for fault in [
        Fault::RecoveryParentCancelledAtReceipt,
        Fault::RecoveryParentCancelledAtAnswer,
    ] {
        assert_parent_recovery(false, Some(fault)).await;
    }
}
#[tokio::test]
async fn public_parent_and_observer_cancellation_stop_close_after_durable_capture() {
    for fault in [
        Fault::RecoveryParentCancelledAtRetirement,
        Fault::RecoveryObserverCancelledAtRetirement,
    ] {
        assert_parent_recovery(false, Some(fault)).await;
    }
}
#[tokio::test]
async fn completed_parent_retry_revokes_close_after_parent_model_or_observer_cancellation() {
    for fault in [
        Fault::RecoveryRetryParentCancelledAtRetirement,
        Fault::RecoveryRetryModelCancelledAtRetirement,
        Fault::RecoveryRetryObserverCancelledAtRetirement,
    ] {
        assert_parent_recovery(false, Some(fault)).await;
    }
}
#[tokio::test]
async fn saved_selected_cleanup_revokes_dead_worker_close_on_parent_model_or_observer_cancellation()
{
    for fault in [
        Fault::RecoveryRetryParentCancelledAtRetirement,
        Fault::RecoveryRetryModelCancelledAtRetirement,
        Fault::RecoveryRetryObserverCancelledAtRetirement,
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_root, manifest) = dead_provider_attempt_state(false, true, Some(port), None);
        let store =
            SessionStore::new(Path::new(manifest["home"].as_str().unwrap()).join(".oracle"));
        let parent_id = manifest["parentId"].as_str().unwrap();
        let child_id = manifest["childId"].as_str().unwrap();
        let original = store.stored_for_update(child_id).unwrap();
        store
            .update(parent_id, |_| {
                Ok(Some(json!({"cleanupWarning":"PUBLIC saved retry"})))
            })
            .unwrap();
        let cancel = CancellationToken::new();
        let trace = Arc::new(Mutex::new(vec![]));
        let sends = Arc::new(AtomicUsize::new(
            manifest["sends"].as_u64().unwrap() as usize
        ));
        let closes = Arc::new(AtomicUsize::new(0));
        let exchange = Exchange {
            fault,
            prompt: request(&store, child_id).unwrap().prompt,
            url: "https://chatgpt.com/c/consult-public".into(),
            owner: original["browser"]["runtime"]["submissionOwner"].clone(),
            commit: Value::Null,
            before: false,
            turns: vec![],
            sends: sends.clone(),
            closes: closes.clone(),
            closed: false,
            trace: trace.clone(),
            cancel: cancel.clone(),
            store: store.clone(),
            id: child_id.into(),
            download_stage: None,
            result_reads: 0,
            recovery_observed: false,
            download_count: 0,
            delayed_image_visible: false,
            retirement_marker: None,
            pending_retirement_read: false,
            cleanup_lock: None,
        };
        let peer = tokio::spawn(async move {
            let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            exchange
                .run(tokio_tungstenite::accept_async(socket).await.unwrap())
                .await;
        });
        assert!(
            crate::browser_attempt::settle_selected_saved(&store, parent_id, &cancel)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(3), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert!(
            !trace
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "Target.createTarget")
        );
        let retained = store.stored_for_update(child_id).unwrap();
        assert_eq!(retained["lifecycle"], original["lifecycle"]);
        assert_eq!(
            retained["browser"]["attempt"],
            original["browser"]["attempt"]
        );
        assert_eq!(
            retained["browser"]["runtime"]["ownedRecoveryTarget"],
            original["browser"]["runtime"]["ownedRecoveryTarget"]
        );
        assert_eq!(retained["recoveryRequired"], true);
    }
}
async fn assert_parent_recovery(sequential: bool, image_fault: Option<Fault>) {
    assert_parent_recovery_from(sequential, image_fault, None).await;
}
#[tokio::test]
async fn original_child_capture_is_reused_after_worker_exit_before_parent_adoption() {
    for (kind, fault) in [
        ("single", Fault::None),
        ("formatted", Fault::FormattedMarkdown),
        ("formatted-image", Fault::FormattedImage),
        ("sequence", Fault::Sequential),
        ("images", Fault::SequentialNamedLast),
        ("buttons", Fault::SequentialNamedButtons),
    ] {
        assert_parent_recovery_from(
            matches!(kind, "sequence" | "images" | "buttons"),
            Some(fault),
            Some(kind),
        )
        .await;
    }
}
#[tokio::test]
async fn selected_parent_resumes_after_worker_exit_without_recapture_or_republication() {
    assert_parent_recovery_from(false, Some(Fault::None), Some("selected")).await;
}
#[tokio::test]
async fn captured_parent_resumes_after_worker_exit_without_recapture() {
    for (kind, fault) in [
        ("captured", Fault::None),
        ("captured-existing", Fault::None),
        ("captured-conflict", Fault::None),
        ("captured-formatted", Fault::FormattedMarkdown),
        ("captured-image", Fault::FormattedImage),
        ("captured-file", Fault::File),
        ("captured-export", Fault::FileExport),
    ] {
        assert_parent_recovery_from(false, Some(fault), Some(kind)).await;
    }
}
#[tokio::test]
async fn remote_parent_recovers_original_capture_and_waits_for_final_ack_before_retirement() {
    for kind in [
        "remote",
        "remote-capture",
        "remote-partial",
        "remote-result",
    ] {
        assert_parent_recovery_from(false, Some(Fault::RemoteCapture), Some(kind)).await;
    }
}
#[tokio::test]
async fn reserved_parent_resumes_completed_files_without_recapture_or_duplicate_publication() {
    for (kind, fault) in [
        ("reserved", Fault::None),
        ("reserved-file", Fault::File),
        ("reserved-parent-file", Fault::File),
        ("reserved-image", Fault::FormattedImage),
        ("reserved-export", Fault::FileExport),
    ] {
        assert_parent_recovery_from(false, Some(fault), Some(kind)).await;
    }
}
async fn assert_parent_recovery_from(
    sequential: bool,
    image_fault: Option<Fault>,
    capture_kind: Option<&str>,
) {
    let (_root, manifest) = dead_provider_attempt_state(sequential, false, None, capture_kind);
    let home = PathBuf::from(manifest["home"].as_str().unwrap());
    let store = SessionStore::new(home.join(".oracle"));
    let parent_id = manifest["parentId"].as_str().unwrap();
    let child_id = manifest["childId"].as_str().unwrap();
    let parent = store.require(parent_id).unwrap();
    let child = store.require(child_id).unwrap();
    let capture_files_for = |id: &str| {
        let mut names = fs::read_dir(store.directory(id).unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("captured-answer-"))
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let capture_files = || capture_files_for(child_id);
    let original_capture_files = capture_files();
    let original_parent_files = capture_files_for(parent_id);
    let original_transport = ["remote-capture.json", "remote-result.json"]
        .into_iter()
        .filter_map(|name| {
            let path = store.directory(parent_id).unwrap().join(name);
            let bytes = fs::read(&path).ok()?;
            let identity = crate::native_fs::NativeEntry::open(&path, false)
                .unwrap()
                .identity;
            Some((path, bytes, identity))
        })
        .collect::<Vec<_>>();
    let pending_artifact_files = || {
        let path = store.directory(parent_id).unwrap().join("artifacts");
        let mut files = if path.exists() {
            fs::read_dir(path)
                .unwrap()
                .map(|row| row.unwrap().path())
                .filter(|path| path.is_file())
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        files.sort();
        files
            .into_iter()
            .map(|path| {
                let entry = crate::native_fs::NativeEntry::open(&path, false).unwrap();
                (path, entry.identity)
            })
            .collect::<Vec<_>>()
    };
    let original_pending_artifacts = pending_artifact_files();
    let original_controller = crate::execution::controller_identity(&parent);
    let original_child_controller = crate::execution::controller_identity(&child);
    let original_link = child["browser"]["attempt"].clone();
    let creator = child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"].clone();
    let journal = TargetJournal::open(&store).unwrap();
    let record = journal.read(&creator["allocationId"]).unwrap();
    assert_eq!(original_controller, original_link["parentController"]);
    assert_eq!(parent["browser"]["consultation"]["active"], original_link);
    assert_eq!(record["nativeOwnerIdentity"], manifest["identity"]);
    assert!(record["commit"].is_null());
    assert_ne!(record["disposition"]["decision"], "retired");
    if capture_kind == Some("selected")
        || capture_kind.is_some_and(|kind| kind.starts_with("remote-"))
    {
        crate::browser_attempt::require_selected(&store, parent_id, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            parent["status"],
            if capture_kind == Some("remote-partial") {
                "partial"
            } else {
                "running"
            }
        );
        if capture_kind == Some("selected") {
            assert!(home.join("PUBLIC-recovered.md").is_file());
        }
    } else if capture_kind.is_some_and(|kind| kind.starts_with("captured")) {
        assert_eq!(
            parent["browser"]["consultation"]["adoption"]["phase"],
            "captured"
        );
        assert!(parent["browser"]["capturedAnswer"].is_object());
        assert_eq!(
            home.join("PUBLIC-recovered.md").exists(),
            matches!(
                capture_kind,
                Some("captured-existing" | "captured-conflict")
            )
        );
    } else {
        if capture_kind.is_some_and(|kind| kind.starts_with("reserved")) {
            assert_eq!(
                parent["browser"]["consultation"]["adoption"]["phase"],
                "reserved"
            );
        }
        assert!(parent["browser"]["capturedAnswer"].is_null());
        assert!(!home.join("PUBLIC-recovered.md").exists());
    }
    let original_output_identity =
        crate::native_fs::NativeEntry::open(&home.join("PUBLIC-recovered.md"), false)
            .ok()
            .map(|entry| entry.identity);
    let profile = ProfileRegistry::open_existing(&home.join("synthetic-profile")).unwrap();
    let (client, server) = tokio::io::duplex(1024 * 1024);
    let browser = Cdp::from_socket(
        WebSocketStream::from_raw_socket(client, Role::Client, None).await,
        Duration::from_secs(1),
    );
    let socket = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let trace = Arc::new(Mutex::new(vec![]));
    let sends = Arc::new(AtomicUsize::new(
        manifest["sends"].as_u64().unwrap() as usize
    ));
    let closes = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let committed = request(&store, child_id).unwrap();
    let mut turns = vec![committed.prompt.clone()];
    turns.extend(crate::browser_turns::prompts(&child["options"]).unwrap());
    let peer = tokio::spawn(
        Exchange {
            fault: image_fault.unwrap_or(if sequential {
                Fault::Sequential
            } else {
                Fault::None
            }),
            prompt: committed.prompt,
            url: "https://chatgpt.com/c/consult-public".into(),
            owner: child["browser"]["runtime"]["submissionOwner"].clone(),
            commit: Value::Null,
            before: false,
            turns: if sequential { turns } else { vec![] },
            sends: sends.clone(),
            closes: closes.clone(),
            closed: false,
            trace: trace.clone(),
            cancel: cancel.clone(),
            store: store.clone(),
            id: child_id.into(),
            download_stage: None,
            result_reads: 0,
            recovery_observed: false,
            download_count: 0,
            delayed_image_visible: false,
            retirement_marker: None,
            pending_retirement_read: false,
            cleanup_lock: None,
        }
        .run(socket),
    );
    let endpoint = AttachmentEndpoint {
        host: "127.0.0.1".into(),
        port: 9222,
        browser_ws_endpoint: "ws://127.0.0.1:9222/devtools/browser/PUBLIC".into(),
    };
    let before_child = store.stored_for_update(child_id).unwrap();
    let direct = recover_existing(
        &store,
        child_id,
        profile.clone(),
        browser.clone(),
        endpoint.clone(),
        &limits(),
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(direct.to_string().contains("original public parent"));
    let direct_snapshot = crate::browser_sessions::HarvestSnapshot {
        endpoint: endpoint.clone(),
        tab: json!({"state":"completed","completionConfirmed":true}),
        integrity: crate::browser_harvest::HarvestIntegrity {
            status: "matched".into(),
            observed_conversation_id: Some("consult-public".into()),
            captured: vec![],
            unverified_sources: vec![],
            explicit_target: false,
            previous_harvest_conversation_id: None,
        },
    };
    assert!(
        crate::browser_historical::complete_owned(&store, child_id, &direct_snapshot, &cancel)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("original public parent")
    );
    assert_eq!(store.stored_for_update(child_id).unwrap(), before_child);
    assert!(trace.lock().unwrap().is_empty());
    let mut recovered = recover_existing(
        &store,
        parent_id,
        profile.clone(),
        browser.clone(),
        endpoint.clone(),
        &limits(),
        &cancel,
    )
    .await;
    let retry_cancel = matches!(
        image_fault,
        Some(
            Fault::RecoveryRetryParentCancelledAtRetirement
                | Fault::RecoveryRetryModelCancelledAtRetirement
                | Fault::RecoveryRetryObserverCancelledAtRetirement
        )
    );
    if retry_cancel {
        assert!(
            recovered.is_err(),
            "First retirement must retain the foreign marker"
        );
        assert_eq!(store.require(parent_id).unwrap()["status"], "completed");
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert!(!cancel.is_cancelled());
        recovered = recover_existing(
            &store,
            parent_id,
            profile.clone(),
            browser.clone(),
            endpoint.clone(),
            &limits(),
            &cancel,
        )
        .await;
    }
    if capture_kind == Some("captured-conflict") {
        assert!(recovered.is_err());
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read(home.join("PUBLIC-recovered.md")).unwrap(),
            b"PUBLIC external answer"
        );
        assert_eq!(
            store.require(parent_id).unwrap()["browser"]["consultation"]["adoption"]["phase"],
            "captured"
        );
        assert_eq!(store.require(child_id).unwrap()["status"], "running");
        fs::rename(
            home.join("PUBLIC-recovered.md"),
            home.join("PUBLIC-external-preserved.md"),
        )
        .unwrap();
        recovered = recover_existing(
            &store,
            parent_id,
            profile.clone(),
            browser.clone(),
            endpoint.clone(),
            &limits(),
            &cancel,
        )
        .await;
    }
    if capture_kind.is_some_and(|kind| kind.starts_with("remote")) {
        let pending = recovered.as_ref().unwrap();
        for (path, bytes, identity) in &original_transport {
            assert_eq!(
                &fs::read(path).unwrap(),
                bytes,
                "Remote recovery replaced immutable transport bytes"
            );
            assert_eq!(
                &crate::native_fs::NativeEntry::open(path, false)
                    .unwrap()
                    .identity,
                identity
            );
        }
        assert_eq!(pending["status"], "running");
        assert_eq!(store.require(child_id).unwrap()["status"], "running");
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        let record =
            crate::remote_result::read(&store, "12345678-1234-1234-1234-123456789abc").unwrap();
        assert_eq!(record.native()["phase"], "result");
        let (acknowledged, committed) =
            crate::remote_result::complete_acknowledgement_lossless(&store, &record).unwrap();
        assert!(committed);
        assert_eq!(acknowledged["status"], "completed");
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        recovered = recover_existing(
            &store,
            parent_id,
            profile.clone(),
            browser.clone(),
            endpoint.clone(),
            &limits(),
            &cancel,
        )
        .await;
    }
    if recovered.is_ok() {
        let repaired = recover_existing(
            &store,
            parent_id,
            profile,
            browser.clone(),
            endpoint.clone(),
            &limits(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(repaired["status"], "completed");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }
    browser
        .disconnect_and_wait(Duration::from_secs(1))
        .await
        .unwrap();
    peer.await.unwrap();
    if retry_cancel
        || matches!(
            image_fault,
            Some(
                Fault::RecoveryParentCancelledAtRetirement
                    | Fault::RecoveryObserverCancelledAtRetirement
            )
        )
    {
        assert!(recovered.is_err());
        let retained = store.stored_for_update(child_id).unwrap();
        let parent = store.stored_for_update(parent_id).unwrap();
        assert!(retained["browser"]["capturedAnswer"].is_object());
        assert_eq!(parent["recoveryRequired"], true);
        assert_eq!(
            parent["status"],
            if matches!(
                image_fault,
                Some(
                    Fault::RecoveryParentCancelledAtRetirement
                        | Fault::RecoveryRetryParentCancelledAtRetirement
                )
            ) {
                "cancelled"
            } else {
                "completed"
            }
        );
        assert!(home.join("PUBLIC-recovered.md").is_file());
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        return;
    }
    if matches!(
        image_fault,
        Some(Fault::RecoveryParentCancelledAtReceipt | Fault::RecoveryParentCancelledAtAnswer)
    ) {
        assert!(recovered.is_err());
        let retained = store.stored_for_update(child_id).unwrap();
        assert!(retained["browser"]["capturedAnswer"].is_null());
        assert_eq!(
            store.stored_for_update(parent_id).unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            crate::execution::controller_identity(&retained),
            original_child_controller
        );
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        if matches!(image_fault, Some(Fault::RecoveryParentCancelledAtReceipt)) {
            assert_eq!(retained["dispatch"]["state"], child["dispatch"]["state"]);
            assert!(retained["browser"]["runtime"]["userCommitReceipt"].is_null());
        }
        assert!(!home.join("PUBLIC-recovered.md").exists());
        return;
    }
    let recovered = recovered.unwrap();
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["recoveryRequired"], false);
    assert_eq!(
        crate::execution::controller_identity(&recovered),
        original_controller
    );
    if capture_kind == Some("selected")
        || capture_kind.is_some_and(|kind| kind.starts_with("captured"))
    {
        assert_eq!(
            recovered["browser"]["capturedAnswer"],
            parent["browser"]["capturedAnswer"]
        );
        if capture_kind == Some("selected") {
            assert_eq!(
                recovered["browser"]["requiredTextOutput"],
                parent["browser"]["requiredTextOutput"]
            );
            assert_eq!(recovered["artifacts"], parent["artifacts"]);
        } else {
            assert!(
                parent["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|row| recovered["artifacts"].as_array().unwrap().contains(row))
            );
            if capture_kind == Some("captured-existing") {
                assert_eq!(
                    crate::native_fs::NativeEntry::open(&home.join("PUBLIC-recovered.md"), false)
                        .unwrap()
                        .identity,
                    original_output_identity.unwrap()
                );
            }
        }
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|row| !row["params"]["expression"]
                    .as_str()
                    .is_some_and(|expression| expression.contains("industrious-dom:recovery"))),
            "Selected-parent resume re-entered provider capture"
        );
    }
    if capture_kind.is_some_and(|kind| kind.starts_with("reserved")) {
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .all(|row| !row["params"]["expression"]
                    .as_str()
                    .is_some_and(|expression| expression.contains("industrious-dom:recovery"))),
            "Reserved parent re-entered provider capture"
        );
        if !original_pending_artifacts.is_empty() {
            // The parent's own transcript is new at adoption; pending artifacts stay exact.
            assert_eq!(
                pending_artifact_files()
                    .into_iter()
                    .filter(|(path, _)| path.file_name().unwrap() != "transcript.md")
                    .collect::<Vec<_>>(),
                original_pending_artifacts,
                "Reserved recovery replaced or duplicated pending artifacts"
            );
        }
        if !original_parent_files.is_empty() {
            assert_eq!(
                capture_files_for(parent_id),
                original_parent_files,
                "Reserved recovery duplicated pending parent capture"
            );
        }
    }
    let settled = store.require(child_id).unwrap();
    assert_eq!(settled["status"], "completed");
    assert_eq!(settled["dispatch"]["state"], "completed");
    assert_eq!(settled["recoveryRequired"], false);
    if capture_kind.is_some() {
        assert_eq!(
            settled["browser"]["capturedAnswer"], child["browser"]["capturedAnswer"],
            "Recovery replaced the original accepted child capture"
        );
        assert_eq!(
            capture_files(),
            original_capture_files,
            "Recovery accumulated another child capture"
        );
    }
    assert_eq!(
        crate::execution::controller_identity(&settled),
        original_child_controller
    );
    assert_eq!(settled["browser"]["attempt"], original_link);
    let retired = journal.read(&creator["allocationId"]).unwrap();
    assert_eq!(retired["creator"], creator);
    assert_eq!(retired["disposition"]["decision"], "retired");
    if !capture_kind.is_some_and(|kind| kind.starts_with("remote")) {
        assert!(
            retired["commit"]["requiredOutputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["file"] == recovered["browser"]["requiredTextOutput"]["file"])
        );
        assert!(
            Path::new(
                recovered["browser"]["requiredTextOutput"]["file"]
                    .as_str()
                    .unwrap()
            )
            .is_file()
        );
    }
    assert_eq!(
        sends.load(Ordering::SeqCst),
        manifest["sends"].as_u64().unwrap() as usize
    );
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert!(trace.lock().unwrap().iter().all(|v| !matches!(
        v["method"].as_str(),
        Some(
            "Target.createTarget"
                | "Page.navigate"
                | "Input.dispatchKeyEvent"
                | "DOM.setFileInputFiles"
        )
    )));
}

#[tokio::test]
async fn parent_adoption_completes_then_retires_only_selected_child_and_duplicate_is_no_close() {
    let mut f = fixture_attempt(
        Fault::File,
        false,
        true,
        json!({"writeOutputPath":"PUBLIC-adopted.md"}),
    )
    .await;
    let child = run_attempt_capture(&f).await.unwrap();
    let parent_id = child["browser"]["attempt"]["parentId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.coordinator
        .as_mut()
        .unwrap()
        .adopt(&f.id, &f.cancel)
        .unwrap();
    assert!(crate::browser_attempt::complete_selected(&f.store, &parent_id, &f.cancel).is_err());
    f.coordinator.as_ref().unwrap().complete(&f.cancel).unwrap();
    crate::browser_attempt::settle_selected(&f.store, &parent_id, &f.cdp, &f.endpoint, &f.cancel)
        .await
        .unwrap();
    let settled = f.store.require(&f.id).unwrap();
    assert_eq!(settled["status"], "completed");
    assert_eq!(settled["dispatch"]["state"], "completed");
    assert_eq!(settled["recoveryRequired"], false);
    assert_eq!(
        f.store.require(&parent_id).unwrap()["recoveryRequired"],
        false
    );
    // The user session, not the provider child, keeps the reference transcript and log block.
    let kinds = |id: &str| {
        f.store.require(id).unwrap()["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|artifact| artifact["kind"] == "transcript")
            .count()
    };
    assert_eq!((kinds(&parent_id), kinds(&f.id)), (1, 0));
    let parent_log = f.store.log(&parent_id).unwrap();
    assert!(parent_log.contains("[browser] Model selection evidence: "));
    assert!(parent_log.contains("[browser] ChatGPT archive skipped (temporary-chat).\n"));
    assert_eq!(
        f.store.require(&parent_id).unwrap()["browser"]["archive"]["reason"],
        "temporary-chat"
    );
    assert!(f.store.require(&f.id).unwrap()["browser"]["archive"].is_null());
    let transcript = f.store.require(&parent_id).unwrap()["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "transcript")
        .unwrap()["path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        fs::read_to_string(transcript)
            .unwrap()
            .contains("\n\nConversation: https://chatgpt.com/c/")
    );
    let journal = TargetJournal::open(&f.store).unwrap();
    let record = journal
        .read(&child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"]["allocationId"])
        .unwrap();
    assert_eq!(record["disposition"]["decision"], "retired");
    assert_eq!(record["disposition"]["absence"], "confirmed-absent");
    assert!(
        record["commit"]["requiredOutputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"]
                == f.store.require(&parent_id).unwrap()["browser"]["requiredTextOutput"]["file"])
    );
    crate::browser_attempt::require_selected(&f.store, &parent_id, &f.cancel).unwrap();
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    crate::browser_attempt::settle_selected(&f.store, &parent_id, &f.cdp, &f.endpoint, &f.cancel)
        .await
        .unwrap();
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    stop(f).await;
}
#[tokio::test]
async fn parent_cleanup_retries_once_without_replaying_or_closing_changed_targets() {
    for (fault, expected_closes, settled) in [
        (Fault::CloseRefusedOnce, 2, true),
        (Fault::CloseRefusedTwice, 2, false),
        (Fault::CloseRefusedForeignMarker, 1, false),
        (Fault::CloseRefusedChangedSource, 0, false),
        (Fault::CloseRefusedActiveThinking, 0, false),
        (Fault::CloseRefusedMissingCompletion, 0, false),
    ] {
        let mut f = fixture_attempt(fault, false, true, json!({})).await;
        let child = run_attempt_capture(&f).await.unwrap();
        let parent_id = child["browser"]["attempt"]["parentId"].as_str().unwrap();
        f.coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .unwrap();
        f.coordinator.as_ref().unwrap().complete(&f.cancel).unwrap();
        let result = crate::browser_attempt::settle_selected(
            &f.store,
            parent_id,
            &f.cdp,
            &f.endpoint,
            &f.cancel,
        )
        .await;
        assert_eq!(result.is_ok(), settled, "{fault:?}: {result:?}");
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            expected_closes,
            "{fault:?}"
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}");
        let parent = f.store.require(parent_id).unwrap();
        assert_eq!(parent["status"], "completed");
        assert_eq!(parent["recoveryRequired"], !settled);
        let journal = TargetJournal::open(&f.store).unwrap();
        let record = journal
            .read(&child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"]["allocationId"])
            .unwrap();
        assert_eq!(
            record["disposition"]["absence"] == "confirmed-absent",
            settled
        );
        if settled {
            crate::browser_attempt::settle_selected(
                &f.store,
                parent_id,
                &f.cdp,
                &f.endpoint,
                &f.cancel,
            )
            .await
            .unwrap();
            assert_eq!(f.closes.load(Ordering::SeqCst), expected_closes);
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn saved_parent_cleanup_reconnects_after_failure_and_lost_ack_but_rechecks_outputs() {
    for mode in ["handshake", "lost-close-ack", "changed-output"] {
        let mut f = fixture_attempt(
            Fault::None,
            false,
            true,
            json!({"writeOutputPath":"PUBLIC-cleanup.md"}),
        )
        .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        f.endpoint.port = port;
        f.endpoint.browser_ws_endpoint = format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC");
        let child = run_attempt_capture(&f).await.unwrap();
        let parent_id = child["browser"]["attempt"]["parentId"]
            .as_str()
            .unwrap()
            .to_owned();
        f.coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .unwrap();
        let parent = f.coordinator.as_ref().unwrap().complete(&f.cancel).unwrap();
        let output = parent["browser"]["requiredTextOutput"]["file"]
            .as_str()
            .unwrap()
            .to_owned();
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = connections.clone();
        let upstream = f.cdp.clone();
        let shutdown = CancellationToken::new();
        let stop_proxy = shutdown.clone();
        let proxy = tokio::spawn(async move {
            for number in 1..=2 {
                let (stream, _) = tokio::select! {
                    biased; _ = stop_proxy.cancelled() => return,
                    connection = listener.accept() => connection.unwrap(),
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                if number == 1 && mode != "lost-close-ack" {
                    if mode == "changed-output" {
                        fs::write(&output, b"PUBLIC changed required output").unwrap();
                    }
                    drop(stream);
                    continue;
                }
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                loop {
                    let message = tokio::select! {
                        biased; _ = stop_proxy.cancelled() => return,
                        message = socket.next() => message,
                    };
                    let Some(Ok(Message::Text(message))) = message else {
                        break;
                    };
                    let request: Value = serde_json::from_str(&message).unwrap();
                    let method = request["method"].as_str().unwrap();
                    let page = request["sessionId"]
                        .as_str()
                        .map_or_else(|| upstream.clone(), |id| upstream.with_session(id.into()));
                    let result = if method == "Target.closeTarget" {
                        page.close_owned_target(
                            request["params"]["targetId"].as_str().unwrap(),
                            Duration::from_secs(1),
                            &stop_proxy,
                        )
                        .await
                    } else {
                        page.call(
                            method,
                            request["params"].clone(),
                            Duration::from_secs(1),
                            &stop_proxy,
                        )
                        .await
                    }
                    .unwrap();
                    if number == 1 && method == "Target.closeTarget" && mode == "lost-close-ack" {
                        // Chrome performs the close, but its acknowledgement never arrives.
                        break;
                    }
                    if socket
                        .send(Message::Text(
                            json!({"id":request["id"],"result":result})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let result =
            crate::browser_attempt::settle_selected_saved(&f.store, &parent_id, &f.cancel).await;
        let settled = mode != "changed-output";
        assert_eq!(result.is_ok(), settled, "{mode}: {result:?}");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            if settled { 2 } else { 1 },
            "{mode}"
        );
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            usize::from(settled),
            "{mode}"
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{mode}");
        assert_eq!(
            f.store.require(&parent_id).unwrap()["recoveryRequired"],
            !settled
        );
        if settled {
            crate::browser_attempt::settle_selected_saved(&f.store, &parent_id, &f.cancel)
                .await
                .unwrap();
            assert_eq!(connections.load(Ordering::SeqCst), 2);
            assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), proxy)
            .await
            .unwrap()
            .unwrap();
        stop(f).await;
    }
}
#[tokio::test]
async fn parent_adoption_commits_required_parent_outputs_to_child_retirement() {
    let mut f = fixture_attempt(
        Fault::File,
        false,
        true,
        json!({"writeOutputPath":"PUBLIC-adopted.md"}),
    )
    .await;
    let child = run_attempt_capture(&f).await.unwrap();
    let parent_id = child["browser"]["attempt"]["parentId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.coordinator
        .as_mut()
        .unwrap()
        .adopt(&f.id, &f.cancel)
        .unwrap();
    f.coordinator.as_ref().unwrap().complete(&f.cancel).unwrap();
    crate::browser_attempt::complete_selected(&f.store, &parent_id, &f.cancel).unwrap();
    fs::write(
        f.home.path().join("PUBLIC-adopted.md"),
        b"PUBLIC changed after child commitment",
    )
    .unwrap();
    let journal = TargetJournal::open(&f.store).unwrap();
    let creator = &child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    assert!(
        crate::browser_retirement::retire_committed(
            &f.store,
            &journal,
            &f.cdp,
            &f.endpoint,
            creator
        )
        .await
        .is_err()
    );
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_ne!(
        journal.read(&creator["allocationId"]).unwrap()["disposition"]["decision"],
        "retired"
    );
    stop(f).await;
}
#[tokio::test]
async fn parent_adoption_refuses_a_child_already_completed_by_the_direct_boundary() {
    let mut f = fixture_attempt(Fault::None, false, true, json!({})).await;
    let child = run(&f).await.unwrap();
    assert_eq!(child["status"], "completed");
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .is_err()
    );
    stop(f).await;
}
#[tokio::test]
async fn parent_adoption_remote_ack_keeps_parent_and_child_identities_and_repairs_duplicates() {
    let mut f = fixture_attempt(Fault::RemoteCapture, false, true, json!({})).await;
    let child = run_attempt_capture(&f).await.unwrap();
    let parent_id = child["browser"]["attempt"]["parentId"]
        .as_str()
        .unwrap()
        .to_owned();
    let run = "12345678-1234-1234-1234-123456789abc";
    f.coordinator
        .as_mut()
        .unwrap()
        .adopt(&f.id, &f.cancel)
        .unwrap();
    let hook: CaptureHook = {
        let store = f.store.clone();
        let id = parent_id.clone();
        Arc::new(move |captured| {
            let saved = store.require(&id)?;
            assert_eq!(saved["browser"]["consultation"]["callbackIssued"], true);
            assert_eq!(saved["browser"]["remoteDelivery"]["captureIssued"], true);
            assert!(captured["ownedRecoveryTarget"].is_null());
            assert_eq!(captured["selectedAttempt"]["link"]["parentId"], id);
            store.update(&id, |_| {
                Ok(Some(json!({"PUBLIC-callback":"outside-parent-lock"})))
            })?;
            let produced = crate::remote_protocol::produced(&store, run, captured, false)?;
            assert!(produced.result["selectedAttempt"].is_null());
            assert!(produced.result["ownedRecoveryTarget"].is_null());
            crate::remote_result::save(
                &store,
                run,
                &produced.captured,
                &produced.result,
                &produced.artifacts,
                crate::remote_result::Phase::Result,
                &CancellationToken::new(),
            )?;
            Ok(())
        })
    };
    f.coordinator
        .as_mut()
        .unwrap()
        .issue_capture(&hook, &f.cancel)
        .unwrap();
    assert!(f.store.require(&f.id).unwrap()["browser"]["remoteDelivery"].is_null());
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .issue_capture(&hook, &f.cancel)
            .is_err()
    );
    let record = crate::remote_result::read(&f.store, run).unwrap();
    let (parent, completed) =
        crate::remote_result::complete_acknowledgement_lossless(&f.store, &record).unwrap();
    assert!(completed);
    assert_eq!(parent["status"], "completed");
    assert_eq!(
        f.store.model_runs(&parent_id).unwrap()[0]["status"],
        "completed"
    );
    assert!(parent.get("dispatch").is_none());
    assert!(parent["browser"]["runtime"]["ownedRecoveryTarget"].is_null());
    assert_eq!(parent["capturedAnswer"]["sessionId"], parent_id);
    assert_eq!(
        f.store.require(&f.id).unwrap()["capturedAnswer"]["sessionId"],
        f.id
    );
    assert_eq!(
        f.store.require(&f.id).unwrap()["dispatch"]["state"],
        "completed"
    );
    crate::browser_attempt::settle_selected(&f.store, &parent_id, &f.cdp, &f.endpoint, &f.cancel)
        .await
        .unwrap();
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    assert!(
        crate::remote_result::finalise_acknowledgement(&f.store, &record)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    stop(f).await;
}
#[tokio::test]
async fn parent_adoption_remote_ack_refuses_tampered_selected_capture() {
    for mutation in ["child", "capture", "creator", "seal", "missing"] {
        let mut f = fixture_attempt(Fault::RemoteCapture, false, true, json!({})).await;
        let child = run_attempt_capture(&f).await.unwrap();
        let id = child["browser"]["attempt"]["parentId"]
            .as_str()
            .unwrap()
            .to_owned();
        let run = "12345678-1234-1234-1234-123456789abc";
        f.coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .unwrap();
        let hook: CaptureHook = Arc::new(|_| Ok(()));
        f.coordinator
            .as_mut()
            .unwrap()
            .issue_capture(&hook, &f.cancel)
            .unwrap();
        let mut captured = crate::remote_protocol::captured(&f.store, &id).unwrap();
        let public = crate::remote_protocol::sanitise(&captured, &[]);
        match mutation {
            "child" => captured["selectedAttempt"]["link"]["childId"] = json!("PUBLIC-foreign"),
            "capture" => captured["answerMarkdown"] = json!("PUBLIC changed capture"),
            "creator" => {
                captured["ownedRecoveryTarget"] =
                    child["browser"]["runtime"]["ownedRecoveryTarget"].clone()
            }
            "seal" => captured["selectedAttempt"]["childEvidenceSha256"] = json!("0".repeat(64)),
            "missing" => {
                captured.as_object_mut().unwrap().remove("selectedAttempt");
            }
            _ => unreachable!(),
        }
        crate::remote_result::save(
            &f.store,
            run,
            &captured,
            &public,
            &json!([]),
            crate::remote_result::Phase::Result,
            &f.cancel,
        )
        .unwrap();
        let record = crate::remote_result::read(&f.store, run).unwrap();
        assert!(
            crate::remote_result::complete_acknowledgement_lossless(&f.store, &record).is_err(),
            "{mutation}"
        );
        assert_eq!(
            f.store.require(&id).unwrap()["status"],
            "running",
            "{mutation}"
        );
        assert_eq!(
            f.store.require(&f.id).unwrap()["dispatch"]["state"],
            "acknowledged",
            "{mutation}"
        );
        assert_eq!(f.closes.load(Ordering::SeqCst), 0, "{mutation}");
        stop(f).await;
    }
}
#[tokio::test]
async fn parent_adoption_failed_remote_callback_is_a_durable_no_retry_barrier() {
    let mut f = fixture_attempt(Fault::RemoteCapture, false, true, json!({})).await;
    let child = run_attempt_capture(&f).await.unwrap();
    let id = child["browser"]["attempt"]["parentId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.coordinator
        .as_mut()
        .unwrap()
        .adopt(&f.id, &f.cancel)
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let hook: CaptureHook = {
        let calls = calls.clone();
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            bail!("PUBLIC callback failure")
        })
    };
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .issue_capture(&hook, &f.cancel)
            .is_err()
    );
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .issue_capture(&hook, &f.cancel)
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.store.require(&id).unwrap()["browser"]["consultation"]["callbackIssued"],
        true
    );
    assert_eq!(
        f.store.require(&f.id).unwrap()["dispatch"]["state"],
        "acknowledged"
    );
    assert!(crate::browser_attempt::complete_selected(&f.store, &id, &f.cancel).is_err());
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    stop(f).await;
}
#[tokio::test]
async fn parent_adoption_owns_a_new_capture_and_published_artifacts_without_closing_child() {
    for fault in [Fault::None, Fault::FileExport, Fault::NamedImage] {
        let mut f = fixture_attempt(
            fault,
            false,
            true,
            json!({"writeOutputPath":"PUBLIC-adopted.md"}),
        )
        .await;
        let child = run_attempt_capture(&f).await.unwrap();
        let parent_id = child["browser"]["attempt"]["parentId"]
            .as_str()
            .unwrap()
            .to_owned();
        let adopted = f
            .coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .unwrap();
        assert_eq!(adopted["id"], parent_id);
        assert_eq!(adopted["status"], "running");
        assert!(adopted.get("dispatch").is_none());
        assert!(adopted["browser"]["runtime"]["ownedRecoveryTarget"].is_null());
        assert_eq!(adopted["capturedAnswer"]["sessionId"], parent_id);
        assert_ne!(adopted["capturedAnswer"], child["capturedAnswer"]);
        let text = String::from_utf8(captured(&f.store, &parent_id).unwrap()).unwrap();
        assert!(text.contains("PUBLIC exact owned answer"));
        let rows = adopted["artifacts"].as_array().unwrap();
        for row in rows
            .iter()
            .filter(|row| matches!(row["kind"].as_str(), Some("file" | "image")))
        {
            assert!(
                !row["path"]
                    .as_str()
                    .unwrap()
                    .starts_with(f.store.directory(&f.id).unwrap().to_str().unwrap())
            );
            crate::browser_output::verify_saved_file(row, &f.cancel).unwrap();
            assert!(row["nativeIdentity"].is_object());
        }
        if matches!(fault, Fault::FileExport) {
            assert!(rows.iter().filter(|row| row["kind"] == "file").count() >= 2);
        }
        if matches!(fault, Fault::NamedImage) {
            assert!(f.home.path().join("PUBLIC-image.png").exists());
        }
        crate::browser_attempt::require_selected(&f.store, &parent_id, &f.cancel).unwrap();
        assert!(crate::browser_attempt::require_child(&f.store, &f.id, &f.cancel).is_err());
        assert!(
            f.coordinator
                .as_mut()
                .unwrap()
                .reserve_first(&f.cancel)
                .is_err()
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.store.require(&f.id).unwrap()["dispatch"]["state"],
            "acknowledged"
        );
        stop(f).await;
    }
}
#[tokio::test]
async fn parent_adoption_failed_output_leaves_capture_barrier_and_retained_child() {
    let mut f = fixture_attempt(
        Fault::None,
        false,
        true,
        json!({"writeOutputPath":"PUBLIC-blocked.md"}),
    )
    .await;
    let child = run_attempt_capture(&f).await.unwrap();
    let parent_id = child["browser"]["attempt"]["parentId"].as_str().unwrap();
    fs::create_dir(f.home.path().join("PUBLIC-blocked.md")).unwrap();
    fs::write(
        f.home.path().join("PUBLIC-blocked.md/PUBLIC-sentinel"),
        b"PUBLIC preserve",
    )
    .unwrap();
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .is_err()
    );
    let parent = f.store.require(parent_id).unwrap();
    assert_eq!(
        parent["browser"]["consultation"]["adoption"]["phase"],
        "captured"
    );
    assert!(parent["browser"]["consultation"]["selected"].is_null());
    assert!(!captured(&f.store, parent_id).unwrap().is_empty());
    assert!(!captured(&f.store, &f.id).unwrap().is_empty());
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .is_err()
    );
    assert!(
        f.coordinator
            .as_mut()
            .unwrap()
            .reserve_first(&f.cancel)
            .is_err()
    );
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read(f.home.path().join("PUBLIC-blocked.md/PUBLIC-sentinel")).unwrap(),
        b"PUBLIC preserve"
    );
    stop(f).await;
}
#[tokio::test]
async fn parent_adoption_rejects_changed_output_capture_child_selection_and_cancellation() {
    for mutation in [
        "output",
        "artifact",
        "parent-capture",
        "child-capture",
        "selection",
        "request",
        "cancel",
    ] {
        let mut f = fixture_attempt(
            Fault::File,
            false,
            true,
            json!({"writeOutputPath":"PUBLIC-adopted.md"}),
        )
        .await;
        let child = run_attempt_capture(&f).await.unwrap();
        let parent_id = child["browser"]["attempt"]["parentId"]
            .as_str()
            .unwrap()
            .to_owned();
        let adopted = f
            .coordinator
            .as_mut()
            .unwrap()
            .adopt(&f.id, &f.cancel)
            .unwrap();
        match mutation {
            "output" => {
                fs::write(f.home.path().join("PUBLIC-adopted.md"), b"PUBLIC altered").unwrap();
            }
            "artifact" => {
                let row = adopted["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["kind"] == "file")
                    .unwrap();
                fs::write(row["path"].as_str().unwrap(), b"PUBLIC altered").unwrap();
            }
            "parent-capture" => {
                fs::write(
                    f.store
                        .directory(&parent_id)
                        .unwrap()
                        .join(adopted["capturedAnswer"]["file"].as_str().unwrap()),
                    b"PUBLIC altered",
                )
                .unwrap();
            }
            "child-capture" => {
                fs::write(
                    f.store
                        .directory(&f.id)
                        .unwrap()
                        .join(child["capturedAnswer"]["file"].as_str().unwrap()),
                    b"PUBLIC altered",
                )
                .unwrap();
            }
            "selection" => {
                f.store.update(&f.id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"thinkingSelection":{"verified":true,"resolvedLabel":"PUBLIC changed"}}))}))})))).unwrap();
            }
            "request" => {
                let mut request = request(&f.store, &f.id).unwrap();
                request.prompt.push_str(" PUBLIC changed");
                request.prompt_sha256 = sha256(request.prompt.as_bytes());
                atomic_json(
                    &f.store
                        .directory(&f.id)
                        .unwrap()
                        .join("browser-request.json"),
                    &request,
                )
                .unwrap();
            }
            "cancel" => f.cancel.cancel(),
            _ => unreachable!(),
        }
        assert!(
            crate::browser_attempt::require_selected(&f.store, &parent_id, &f.cancel).is_err(),
            "{mutation}"
        );
        assert_eq!(f.closes.load(Ordering::SeqCst), 0, "{mutation}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{mutation}");
        stop(f).await;
    }
}
#[tokio::test]
async fn bounded_provider_read_recovers_late_commitment_and_answers_without_replay() {
    for fault in [
        Fault::ProviderLateCommit,
        Fault::ProviderLateAnswer,
        Fault::ProviderLateAnswerDeadline,
    ] {
        let f = fixture(fault, false).await;
        let result = run_within(&f, &recovery_limits())
            .await
            .unwrap_or_else(|error| panic!("{fault:?}: {error:#}"));
        assert_eq!(result["status"], "completed", "{fault:?}");
        assert_eq!(result["recoveryRequired"], false, "{fault:?}");
        assert_eq!(
            result["browser"]["runtime"]["providerReadRecovery"]["state"],
            "recovered"
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}");
        assert_eq!(f.closes.load(Ordering::SeqCst), 1, "{fault:?}");
        {
            let trace = f.trace.lock().unwrap();
            assert_eq!(
                trace
                    .iter()
                    .filter(|request| request["method"] == "Target.createTarget")
                    .count(),
                1
            );
            assert_eq!(
                trace
                    .iter()
                    .filter(|request| request["method"] == "Page.navigate")
                    .count(),
                1
            );
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn bounded_provider_read_rejects_changed_source_request_owner_privacy_and_cancellation() {
    for fault in [
        Fault::ProviderRecoveryWrongSource,
        Fault::ProviderRecoveryChangedRequest,
        Fault::ProviderRecoveryChangedOwner,
        Fault::ProviderRecoveryCancelled,
        Fault::ProviderRecoveryPrivacy,
    ] {
        let f = fixture(fault, false).await;
        let error = run_within(&f, &recovery_limits()).await.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::browser_provider::Failure>()
                .is_none(),
            "{fault:?}: {error:#}"
        );
        assert!(captured(&f.store, &f.id).is_err(), "{fault:?}");
        assert_ne!(f.store.require(&f.id).unwrap()["status"], "completed");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}");
        // Reference 15c8b9a: only a cancelled run closes its own settled tab.
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            usize::from(f.cancel.is_cancelled()),
            "{fault:?}"
        );
        stop(f).await;
    }
}
#[tokio::test]
async fn bounded_provider_read_keeps_recovered_child_until_parent_adoption() {
    let mut f = fixture_attempt(Fault::ProviderLateAnswer, false, true, json!({})).await;
    let child = run_attempt_capture_within(&f, &recovery_limits())
        .await
        .unwrap();
    assert_eq!(child["status"], "running");
    assert_eq!(child["dispatch"]["state"], "acknowledged");
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    f.coordinator
        .as_mut()
        .unwrap()
        .adopt(&f.id, &f.cancel)
        .unwrap();
    f.coordinator.as_mut().unwrap().complete(&f.cancel).unwrap();
    let parent_id = child["browser"]["attempt"]["parentId"]
        .as_str()
        .unwrap()
        .to_owned();
    crate::browser_attempt::settle_selected(&f.store, &parent_id, &f.cdp, &f.endpoint, &f.cancel)
        .await
        .unwrap();
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    assert_eq!(f.store.require(&parent_id).unwrap()["status"], "completed");
    stop(f).await;
}
#[tokio::test]
async fn bounded_provider_read_does_not_restart_full_answer_timeout_after_commit_recovery() {
    let f = fixture(Fault::ProviderLateCommitPendingAnswer, false).await;
    let mut bounded = recovery_limits();
    bounded.answer = Duration::from_secs(60);
    // The commit wait and the recovered read share about 2 x 3 s; a restarted answer
    // timeout would run for 60 s.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        execute(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &f.identity,
            &bounded,
            &f.cancel,
        ),
    )
    .await
    .expect("recovered commitment restarted the full answer timeout");
    let error = result.unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<crate::browser_provider::Failure>(),
            Some(crate::browser_provider::Failure::AnswerDeadline)
        ),
        "{error:#}"
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert!(captured(&f.store, &f.id).is_err());
    stop(f).await;
}
#[tokio::test]
async fn bounded_provider_read_caps_recovered_ack_and_revalidates_used_episode() {
    for fault in [
        Fault::ProviderLateCommitLostAck,
        Fault::ProviderLateCommitChangedOwner,
    ] {
        let f = fixture(fault, false).await;
        let mut bounded = recovery_limits();
        bounded.command = Duration::from_secs(60);
        bounded.answer = Duration::from_secs(60);
        // The commit wait and the recovered acknowledgement share about 2 x 3 s; an
        // uncapped acknowledgement would wait for the 60 s command timeout.
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                &f.store,
                &f.id,
                f.profile.clone(),
                f.cdp.clone(),
                f.endpoint.clone(),
                &f.identity,
                &bounded,
                &f.cancel,
            ),
        )
        .await
        .expect("recovered acknowledgement exceeded the shared budget");
        let error = result.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::browser_provider::Failure>()
                .is_none(),
            "{fault:?}: {error:#}"
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}");
        assert_eq!(f.closes.load(Ordering::SeqCst), 0, "{fault:?}");
        assert!(captured(&f.store, &f.id).is_err(), "{fault:?}");
        stop(f).await;
    }
}
#[tokio::test]
async fn response_load_error_is_not_captured_and_retains_the_committed_target() {
    let f = fixture(Fault::ResponseLoadFailed, false).await;
    let result = run(&f).await.unwrap_err();
    assert!(
        matches!(
            result.downcast_ref::<crate::browser_provider::Failure>(),
            Some(crate::browser_provider::Failure::ResponseLoadFailed)
        ),
        "{result:#}"
    );
    let meta = f.store.require(&f.id).unwrap();
    assert_ne!(meta["status"], "completed");
    assert_eq!(meta["recoveryRequired"], true);
    assert!(captured(&f.store, &f.id).is_err());
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    stop(f).await;
}
#[tokio::test]
async fn answerless_turns_are_refused_with_the_reference_failures() {
    for fault in [
        Fault::ResponseLoadFailedBesideImage,
        Fault::ContentWithheld,
        Fault::RetryOffered,
    ] {
        let f = fixture(fault, false).await;
        let result = run(&f).await.unwrap_err();
        let failure = result.downcast_ref::<crate::browser_provider::Failure>();
        match fault {
            Fault::ResponseLoadFailedBesideImage => assert!(
                matches!(
                    failure,
                    Some(crate::browser_provider::Failure::ResponseLoadFailed)
                ),
                "{result:#}"
            ),
            Fault::ContentWithheld => assert_eq!(
                result.to_string(),
                "ChatGPT finished the submitted assistant turn without showing an answer: This content can't be shown"
            ),
            _ => assert_eq!(
                result.to_string(),
                "ChatGPT could not generate the submitted assistant turn and offered Retry."
            ),
        }
        let meta = f.store.require(&f.id).unwrap();
        assert_ne!(meta["status"], "completed", "{fault:?}");
        assert_eq!(meta["recoveryRequired"], true, "{fault:?}");
        assert!(captured(&f.store, &f.id).is_err(), "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}");
        assert_eq!(f.closes.load(Ordering::SeqCst), 0, "{fault:?}");
        stop(f).await;
    }
}
#[tokio::test]
async fn parent_attempt_capture_keeps_its_own_tab_until_adoption() {
    let f = fixture_attempt(Fault::None, false, true, json!({})).await;
    let result = execute_with_boundary(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &f.identity,
        &limits(),
        &f.cancel,
        &CaptureBoundary::ParentAttempt,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "running");
    assert_eq!(result["dispatch"]["state"], "acknowledged");
    assert_eq!(result["browser"]["capturedAnswer"]["sessionId"], f.id);
    assert!(!captured(&f.store, &f.id).unwrap().is_empty());
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    let creator = &result["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let journal = TargetJournal::open(&f.store)
        .unwrap()
        .read(&creator["allocationId"])
        .unwrap();
    assert!(journal["commit"].is_null());
    assert_ne!(journal["disposition"]["decision"], "retired");
    stop(f).await;
}
#[tokio::test]
async fn only_the_provider_answer_deadline_has_a_typed_timeout() {
    for fault in [Fault::Incomplete, Fault::WrongTier, Fault::CancelOutcome] {
        let f = fixture(fault, false).await;
        let error = run_within(&f, &recovery_limits()).await.unwrap_err();
        assert_eq!(
            matches!(
                error.downcast_ref::<crate::browser_provider::Failure>(),
                Some(crate::browser_provider::Failure::AnswerDeadline)
            ),
            matches!(fault, Fault::Incomplete),
            "{fault:?}: {error:#}"
        );
        assert!(captured(&f.store, &f.id).is_err());
        stop(f).await;
    }
}
#[tokio::test]
async fn failed_commit_recovery_persistence_removes_provider_eligibility() {
    for fail_save in [false, true] {
        let (client, server) = tokio::io::duplex(32 * 1024);
        let page = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut socket = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let peer = tokio::spawn(async move {
            while let Some(Ok(Message::Text(frame))) = socket.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let expression = request["params"]["expression"].as_str().unwrap();
                let value = if expression.contains("industrious-dom:submission-receipt") {
                    json!({"probe":{"currentUrl":"https://chatgpt.com/c/PUBLIC-conversation","workSelected":false,"modeUnverified":false,"invalid":false,"turnIds":["PUBLIC-prior-turn"],"messages":[],"canonicalPrompt":"PUBLIC complete source"},"submission":{"state":"none"}})
                } else {
                    json!(true)
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":{"result":{"value":value}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let cancel = CancellationToken::new();
        let reservation = RendererReservation::acquire(
            page.clone(),
            "PUBLIC-claim".into(),
            Duration::from_secs(1),
            &cancel,
        )
        .await
        .unwrap();
        let before:CommitSnapshot=serde_json::from_value(json!({"owner":reservation.owner(),"navigation":{"origin":"https://chatgpt.com","conversationId":"PUBLIC-conversation","landingPath":null},"messageIds":["PUBLIC-prior-user"],"turnIds":["PUBLIC-prior-turn"],"canonicalPrompt":"PUBLIC complete source"})).unwrap();
        let mut budget = limits();
        budget.commit = Duration::ZERO;
        let error = wait_receipt(
            &reservation,
            &before,
            "PUBLIC complete source",
            &budget,
            &cancel,
            |conversation| {
                assert_eq!(conversation, "PUBLIC-conversation");
                if fail_save {
                    bail!("PUBLIC persistence failure");
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            matches!(
                error.downcast_ref::<crate::browser_provider::Failure>(),
                Some(crate::browser_provider::Failure::CommitDeadline)
            ),
            !fail_save,
            "{error:#}"
        );
        if fail_save {
            assert!(error.to_string().contains("PUBLIC persistence failure"));
        }
        page.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
    }
}
#[tokio::test]
async fn provider_attempt_rechecks_parent_after_renderer_waits_without_sending() {
    for fault in [
        Fault::AttemptParentBeforeIntent,
        Fault::AttemptParentBeforeSend,
    ] {
        let f = fixture(fault, false).await;
        let result = run(&f).await;
        assert!(result.is_err(), "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 0, "{fault:?}");
        let child = f.store.require(&f.id).unwrap();
        let parent_id = child["browser"]["attempt"]["parentId"].as_str().unwrap();
        assert_eq!(f.store.require(parent_id).unwrap()["status"], "cancelled");
        assert_eq!(
            child["dispatch"]["state"],
            if matches!(fault, Fault::AttemptParentBeforeSend) {
                "uncertain"
            } else {
                "not-started"
            }
        );
        if matches!(fault, Fault::AttemptParentBeforeIntent) {
            // Composer population seals the existing conservative retention barrier.
            assert_eq!(f.closes.load(Ordering::SeqCst), 0);
            assert_eq!(child["recoveryRequired"], true);
        } else {
            // A durable started intent remains uncertain, even though the Send was refused.
            assert_eq!(f.closes.load(Ordering::SeqCst), 0);
            assert_eq!(child["dispatch"]["state"], "uncertain");
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn provider_attempt_rechecks_parent_after_bringup_and_releases_unissued_lease() {
    let mut f = fixture(Fault::None, false).await;
    let parent_id = f.id.clone();
    let mut coordinator =
        crate::browser_attempt::Parent::begin(&f.store, &parent_id, &f.identity, &f.cancel)
            .unwrap();
    f.id = coordinator.reserve_first(&f.cancel).unwrap().child_id;
    start_owner(&f.store, &f.id, &f.identity).unwrap();
    let held = DirectoryLock::acquire(
        &f.profile.path().join("oracle-browser-bringup.lock"),
        Duration::from_secs(1),
    )
    .unwrap();
    let profile = f.profile.clone();
    let store = f.store.clone();
    let child_id = f.id.clone();
    let observe = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if profile.records().unwrap()["leases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["sessionId"] == child_id)
                {
                    store
                        .update(&parent_id, |_| Ok(Some(json!({"status":"cancelled"}))))
                        .unwrap();
                    held.release().unwrap();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    });
    let result = run(&f).await;
    observe.await.unwrap();
    assert!(result.is_err());
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .all(|row| row["method"] != "Target.createTarget")
    );
    assert!(
        f.profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.store.require(&f.id).unwrap()["recoveryRequired"], false);
    stop(f).await;
}

#[tokio::test]
async fn copied_cleanup_receipt_preserves_completion_and_refuses_a_later_outcome() {
    let f = fixture(Fault::None, false).await;
    run(&f).await.unwrap();
    let witness = ExecutionWitness::capture(&f.store, &f.id, &f.identity).unwrap();
    let settled = f.store.stored_for_update(&f.id).unwrap();
    assert_eq!(settled["status"], "completed");
    let saved = copied_cleanup_receipt(&f.store, &f.id, &witness, &settled, true).unwrap();
    assert_eq!(saved["status"], "completed");
    assert_eq!(saved["dispatch"], settled["dispatch"]);
    assert_eq!(
        saved["browser"]["capturedAnswer"],
        settled["browser"]["capturedAnswer"]
    );
    assert_eq!(
        saved["browser"]["runtime"]["copiedProfileCleanup"]["removed"],
        true
    );
    let settled = f.store.stored_for_update(&f.id).unwrap();
    f.store
        .update(&f.id, |_| Ok(Some(json!({"PUBLIC-later-result":true}))))
        .unwrap();
    let changed = f.store.stored_for_update(&f.id).unwrap();
    assert!(copied_cleanup_receipt(&f.store, &f.id, &witness, &settled, false).is_err());
    assert_eq!(f.store.stored_for_update(&f.id).unwrap(), changed);
    let settled = changed;
    f.store
        .update(&f.id, |_| {
            Ok(Some(json!({"startedAt":"PUBLIC-new-controller"})))
        })
        .unwrap();
    let changed = f.store.stored_for_update(&f.id).unwrap();
    assert!(copied_cleanup_receipt(&f.store, &f.id, &witness, &settled, false).is_err());
    assert_eq!(f.store.stored_for_update(&f.id).unwrap(), changed);
    stop(f).await;
}
#[test]
fn durable_copy_disposal_preserves_copy_receipts_and_recovery_identity() {
    let original = json!({"browser":{"config":{"copyProfileSource":"PUBLIC-source"},
        "runtime":{"ownedRecoveryTarget":{"creator":"PUBLIC-original"},"freshProfileOwner":{"sessionId":"PUBLIC-child"}}}});
    for disposition in ["removed", "retained"] {
        let runtime =
            fresh_disposal_runtime(&original, &json!({"version":1,"disposition":disposition}));
        assert_eq!(
            runtime["copiedProfileCleanup"]["removed"],
            disposition == "removed"
        );
        assert_eq!(
            runtime["recoveryUnavailable"],
            if disposition == "removed" {
                "copied-profile-removed"
            } else {
                "copied-profile-cleanup-unconfirmed"
            }
        );
        assert_eq!(
            runtime["ownedRecoveryTarget"],
            original["browser"]["runtime"]["ownedRecoveryTarget"]
        );
        assert_eq!(
            runtime["freshProfileOwner"],
            original["browser"]["runtime"]["freshProfileOwner"]
        );
    }
    let ordinary = json!({"browser":{"runtime":{}}});
    let runtime = fresh_disposal_runtime(&ordinary, &json!({"version":1,"disposition":"removed"}));
    assert!(runtime.get("copiedProfileCleanup").is_none());
}
#[tokio::test]
async fn completed_profile_receipts_and_failure_handling_do_not_restart_long_lock_waits() {
    let f = fixture(Fault::None, false).await;
    run(&f).await.unwrap();
    let witness = ExecutionWitness::capture(&f.store, &f.id, &f.identity).unwrap();
    let settled = f.store.stored_for_update(&f.id).unwrap();
    let held = DirectoryLock::acquire(
        &f.store
            .home()
            .join("sessions-write-locks")
            .join(format!("{}.lock", f.id)),
        Duration::from_secs(1),
    )
    .unwrap();
    let started = std::time::Instant::now();
    assert!(copied_cleanup_receipt(&f.store, &f.id, &witness, &settled, false).is_err());
    assert!(
        fresh_profile_receipt(
            &f.store,
            &f.id,
            &witness,
            &settled,
            json!({"version":1,"disposition":"retained"})
        )
        .is_err()
    );
    let after = fail_owned(&f.store, &f.id, &witness, |_| {
        panic!("Completed output was terminalised again")
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(after["status"], "completed");
    assert_eq!(f.store.stored_for_update(&f.id).unwrap(), settled);
    assert!(!captured(&f.store, &f.id).unwrap().is_empty());
    held.release().unwrap();
    stop(f).await;
}

#[tokio::test]
async fn cookie_bootstrap_precedes_navigation_and_zero_writes_never_navigate_or_send() {
    for (fault, keep, close, retired) in [
        (Fault::CookieBootstrap, false, None, true),
        (Fault::CookieRejected, true, None, true),
        (Fault::CookieCancelled, false, None, true),
        (Fault::CookieCancelled, true, None, false),
        (Fault::CookieCancelled, true, Some(true), true),
        (Fault::CookieCancelled, false, Some(false), false),
        (Fault::CookieJournalFailure, false, None, false),
    ] {
        let f = fixture(fault, false).await;
        f.store
            .update(&f.id, |meta| {
                let mut config = meta["browser"]["config"].clone();
                config["keepBrowser"] = json!(keep);
                if let Some(close) = close {
                    config["closeOwnedTabOnCancel"] = json!(close);
                }
                Ok(Some(
                    json!({"browser":{"config":config,"runtime":meta["browser"]["runtime"]}}),
                ))
            })
            .unwrap();
        let batch=crate::browser_auth::CookieBootstrap::resolve_explicit(&json!({"browserInlineCookies":r#"[{"name":"PUBLIC-cookie","value":"PUBLIC-cookie-sentinel"}]"#}),&json!({}),&Environment::new(),f.home.path()).unwrap().unwrap();
        let lock = DirectoryLock::acquire(
            &f.profile.path().join("oracle-browser-bringup.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let result = execute_with_bootstrap(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &f.identity,
            &limits(),
            &f.cancel,
            None,
            Some(&batch),
            Some(lock),
            None,
        )
        .await;
        let meta = f.store.require(&f.id).unwrap();
        assert!(!meta.to_string().contains("PUBLIC-cookie-sentinel"));
        let trace = f.trace.lock().unwrap().clone();
        let seeded = trace
            .iter()
            .position(|request| request["method"] == "Network.setCookie")
            .unwrap();
        let navigation = trace
            .iter()
            .position(|request| request["method"] == "Page.navigate");
        if !matches!(fault, Fault::CookieBootstrap) {
            assert!(result.unwrap_err().to_string().contains(
                if matches!(fault, Fault::CookieCancelled) {
                    "cancelled"
                } else {
                    "No inline cookies were applied; aborting before navigation."
                }
            ));
            assert!(navigation.is_none());
            assert_eq!(f.sends.load(Ordering::SeqCst), 0);
            assert_eq!(meta["recoveryRequired"], !retired);
            assert_eq!(
                f.closes.load(Ordering::SeqCst),
                usize::from(retired || matches!(fault, Fault::CookieJournalFailure))
            );
            assert_eq!(
                meta["browser"]["runtime"]["targetDispositionPending"],
                !retired
            );
            assert!(meta["browser"]["runtime"]["ownedRecoveryTarget"].is_object());
            if matches!(fault, Fault::CookieJournalFailure) {
                assert!(meta["browser"]["runtime"]["preNavigationCleanupWarning"].is_string());
                assert!(
                    f.profile.records().unwrap()["leases"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
                let path = f.store.home().join("target-dispositions").join(format!(
                    "{}.json",
                    creator["allocationId"].as_str().unwrap()
                ));
                fs::remove_dir(&path).unwrap();
                fs::rename(path.with_extension("PUBLIC-pending"), &path).unwrap();
                let journal = TargetJournal::open(&f.store).unwrap();
                let mut record = journal.read(&creator["allocationId"]).unwrap();
                assert_eq!(
                    record["disposition"]["reason"],
                    "settled-pre-dispatch-close-pending"
                );
                assert!(record["commit"].is_null());
                record["nativeOwnerIdentity"]["pid"] = json!(u32::MAX);
                record["creator"]["controllerPid"] = json!(u32::MAX);
                crate::storage::atomic_json(&path, &record).unwrap();
                f.store.update(&f.id, |meta| {
                    let mut runtime = meta["browser"]["runtime"].clone();
                    runtime["controllerPid"] = json!(u32::MAX);
                    runtime["ownedRecoveryTarget"]["creator"] = record["creator"].clone();
                    let mut lifecycle = meta["lifecycle"].clone();
                    lifecycle["workerPid"] = json!(u32::MAX);
                    Ok(Some(json!({"lifecycle":lifecycle,"browser":{"config":meta["browser"]["config"],"runtime":runtime}})))
                }).unwrap();
                crate::browser_retirement::reconcile(&f.store, &f.cdp, &f.endpoint)
                    .await
                    .unwrap();
                let disposed = journal.read(&creator["allocationId"]).unwrap();
                assert_eq!(disposed["disposition"]["decision"], "retired");
                assert_eq!(disposed["disposition"]["absence"], "confirmed-absent");
                let settled = f.store.require(&f.id).unwrap();
                assert_eq!(settled["status"], meta["status"]);
                assert_eq!(settled["errorMessage"], meta["errorMessage"]);
                assert_eq!(settled["recoveryRequired"], false);
                assert_eq!(
                    settled["browser"]["runtime"]["targetDispositionPending"],
                    false
                );
                assert_eq!(
                    settled["browser"]["runtime"]["preNavigationCleanupPending"],
                    false
                );
                assert_eq!(f.closes.load(Ordering::SeqCst), 1);
                assert_eq!(f.sends.load(Ordering::SeqCst), 0);
            }
        } else {
            assert_eq!(result.unwrap()["status"], "completed");
            assert!(seeded < navigation.unwrap());
            assert_eq!(meta["browser"]["runtime"]["cookieBootstrap"]["applied"], 1);
            assert!(
                f.store
                    .log(&f.id)
                    .unwrap()
                    .contains("Applied 1 inline cookies")
            );
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
            assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn remote_capture_precedes_completion_and_retirement_and_survives_hook_failure() {
    const RUN: &str = "12345678-1234-1234-1234-123456789abc";
    for reject in [false, true] {
        let f = fixture(
            if reject {
                Fault::RemoteCaptureRejected
            } else {
                Fault::RemoteCapture
            },
            false,
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hook: CaptureHook = {
            let store = f.store.clone();
            let id = f.id.clone();
            let closes = f.closes.clone();
            let calls = calls.clone();
            Arc::new(move |captured| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(store.require(&id)?["status"], "running");
                assert_eq!(
                    store.require(&id)?["browser"]["remoteDelivery"]["captureIssued"],
                    true
                );
                // Acquiring the same write lock proves the external hook runs outside it.
                store.update(&id, |_| {
                    Ok(Some(json!({"PUBLIC-capture-hook-entered":true})))
                })?;
                assert_eq!(closes.load(Ordering::SeqCst), 0);
                let produced = crate::remote_protocol::produced(&store, RUN, captured, false)?;
                crate::remote_result::save(
                    &store,
                    RUN,
                    &produced.captured,
                    &produced.result,
                    &produced.artifacts,
                    crate::remote_result::Phase::Capture,
                    &CancellationToken::new(),
                )?;
                if reject {
                    bail!("PUBLIC remote capture rejected after durable save");
                }
                Ok(())
            })
        };
        let result = execute_with_capture(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &f.identity,
            &limits(),
            &f.cancel,
            Some(&hook),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{result:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        let record = crate::remote_result::read(&f.store, RUN).unwrap();
        assert_eq!(record.native()["phase"], "capture");
        assert!(
            !record.native()["result"]["answerMarkdown"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        if reject {
            assert!(result.unwrap_err().to_string().contains("capture rejected"));
        } else {
            assert_eq!(result.unwrap()["status"], "running");
            let owned = &record.native()["capturedResult"]["ownedRecoveryTarget"]["creator"];
            assert!(
                TargetJournal::open(&f.store)
                    .unwrap()
                    .read(&owned["allocationId"])
                    .unwrap()["commit"]
                    .is_null()
            );
            crate::remote_result::save(
                &f.store,
                RUN,
                &record.native()["capturedResult"],
                &record.native()["result"],
                &record.native()["artifacts"],
                crate::remote_result::Phase::Result,
                &CancellationToken::new(),
            )
            .unwrap();
            let final_record = crate::remote_result::read(&f.store, RUN).unwrap();
            let (session, committed) =
                crate::remote_result::complete_acknowledgement_lossless(&f.store, &final_record)
                    .unwrap();
            assert!(committed);
            assert_eq!(session["status"], "completed");
            assert_eq!(
                session["browser"]["remoteDelivery"]["producerFinished"],
                true
            );
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn allocated_target_creator_survives_the_first_inspection_failure() {
    let f = fixture(Fault::InitialTargetInspection, false).await;
    assert!(run(&f).await.is_err());
    let metadata = f.store.require(&f.id).unwrap();
    let creator = &metadata["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    assert_eq!(creator["target"]["targetId"], "TARGET-PUBLIC");
    assert_eq!(
        metadata["browser"]["runtime"]["creatorPersistenceIncomplete"],
        false
    );
    let journal = TargetJournal::open(&f.store).unwrap();
    assert_eq!(
        journal.read(&creator["allocationId"]).unwrap()["creator"],
        *creator
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["method"] != "Page.navigate")
    );
    assert_eq!(
        f.profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    stop(f).await;
}

#[tokio::test]
async fn completion_refuses_changed_target_options_terminal_model_and_cancellation_without_late_writes()
 {
    for fault in [
        Fault::ReplacedOutcomeTarget,
        Fault::ChangedOutcomeOptions,
        Fault::StoppedOutcomeModel,
        Fault::CancelOutcome,
    ] {
        let f = fixture(fault, false).await;
        let failure = run(&f).await.unwrap_err();
        let metadata = f.store.require(&f.id).unwrap();
        assert_ne!(metadata["status"], "completed");
        assert!(metadata["browser"]["capturedAnswer"].is_null());
        // Reference 15c8b9a: a cancelled run closes its own settled tab and frees its slot.
        let cancelled = f.cancel.is_cancelled();
        assert_eq!(metadata["recoveryRequired"], !cancelled);
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}: {failure:#}");
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(cancelled));
        assert!(!f.home.path().join("PUBLIC-foreign-output.md").exists());
        assert!(
            !f.store
                .log(&f.id)
                .unwrap()
                .contains("PUBLIC exact owned answer")
        );
        if matches!(fault, Fault::StoppedOutcomeModel) {
            assert_eq!(metadata["status"], "error");
            assert!(metadata["completedAt"].is_string());
            assert_eq!(metadata["models"][0]["status"], "partial");
            assert_eq!(
                metadata["models"][0]["marker"],
                "PUBLIC winning model outcome"
            );
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn sequential_browser_turns_use_distinct_receipts_one_upload_and_one_final_transcript() {
    for fault in [
        Fault::Sequential,
        Fault::SequentialExtraHigh,
        Fault::SequentialProModel,
    ] {
        let f = fixture(fault, true).await;
        run(&f).await.unwrap();
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "completed");
        assert_eq!(meta["recoveryRequired"], false);
        assert_eq!(f.sends.load(Ordering::SeqCst), 3);
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        assert_eq!(meta["dispatch"]["turnIndex"], 2);
        let history = meta["dispatch"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["userMessageId"], "user-public-1");
        assert_eq!(history[1]["userMessageId"], "user-public-2");
        assert_ne!(
            history[0]["submissionOwner"]["reservationId"],
            history[1]["submissionOwner"]["reservationId"]
        );
        assert_ne!(
            history[1]["submissionOwner"]["reservationId"],
            meta["dispatch"]["submissionOwner"]["reservationId"]
        );
        assert_eq!(
            meta["browser"]["runtime"]["userCommitReceipt"]["messageId"],
            "user-public-3"
        );
        assert_eq!(
            meta["browser"]["runtime"]["acceptedObservation"]["text"],
            "PUBLIC owned answer 3"
        );
        let receipt: CaptureReceipt =
            serde_json::from_value(meta["browser"]["capturedAnswer"].clone()).unwrap();
        let text =
            String::from_utf8(artifacts::verify_capture(&f.store, &receipt).unwrap()).unwrap();
        assert_eq!(
            text,
            "## Initial response\n\n### Answer\n\nPUBLIC owned answer 1\n\n## Follow-up 1\n\n### Prompt\n\nPUBLIC second turn\n\n### Answer\n\nPUBLIC owned answer 2\n\n## Follow-up 2\n\n### Prompt\n\nPUBLIC third turn\n\n### Answer\n\nPUBLIC owned answer 3"
        );
        let heading = meta["browser"]["requiredTextOutput"]["answerHeading"]
            .as_str()
            .unwrap();
        assert_eq!(
            fs::read_to_string(f.home.path().join("PUBLIC-sequence.md")).unwrap(),
            format!("{heading}\n\n{text}\n")
        );
        {
            let trace = f.trace.lock().unwrap();
            assert_eq!(
                trace
                    .iter()
                    .filter(|r| r["method"] == "Target.createTarget")
                    .count(),
                1
            );
            assert_eq!(
                trace
                    .iter()
                    .filter(|r| r["method"] == "Page.navigate")
                    .count(),
                1
            );
            assert_eq!(
                trace
                    .iter()
                    .filter(|r| r["method"] == "DOM.setFileInputFiles")
                    .count(),
                1
            );
            let expressions = trace
                .iter()
                .filter_map(|r| r["params"]["expression"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                expressions
                    .iter()
                    .filter(|e| e.contains("industrious-dom:thinking")
                        && e.contains("\"inspectionOnly\":false")
                        && e.contains("\"level\":\"pro\""))
                    .count(),
                1
            );
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn sequential_browser_failure_stops_the_remaining_turns_and_retains_recovery() {
    for fault in [
        Fault::SequentialWrongModel,
        Fault::SequentialExtraHighProModel,
        Fault::SequentialWrongTier,
        Fault::SequentialChangedPrior,
        Fault::SequentialLostAck,
        Fault::SequentialCancel,
        Fault::SequentialTimeout,
    ] {
        let f = fixture(fault, false).await;
        assert!(run(&f).await.is_err(), "Expected {fault:?} failure");
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(
            meta["status"],
            if matches!(fault, Fault::SequentialCancel) {
                "cancelled"
            } else {
                "error"
            }
        );
        let cancelled = matches!(fault, Fault::SequentialCancel);
        assert_eq!(meta["recoveryRequired"], !cancelled);
        assert_eq!(meta["dispatch"]["state"], "uncertain");
        assert_eq!(
            f.sends.load(Ordering::SeqCst),
            if matches!(
                fault,
                Fault::SequentialWrongModel
                    | Fault::SequentialExtraHighProModel
                    | Fault::SequentialWrongTier
                    | Fault::SequentialChangedPrior
            ) {
                1
            } else {
                2
            }
        );
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(cancelled));
        assert!(meta["browser"]["capturedAnswer"].is_null());
        assert!(!f.home.path().join("PUBLIC-sequence.md").exists());
        stop(f).await;
    }
}

/// Live receipt L3: the first answer's view was briefly unmounted after Send and the
/// unknown predicate refused the turn. Every turn here starts with an unmounted view.
#[tokio::test]
async fn sequential_browser_turns_wait_for_an_unmounted_temporary_chat_view() {
    let f = fixture(Fault::SequentialPrivacySettling, false).await;
    run(&f).await.unwrap();
    let meta = f.store.require(&f.id).unwrap();
    assert_eq!(meta["status"], "completed");
    assert_eq!(f.sends.load(Ordering::SeqCst), 3);
    assert_eq!(meta["dispatch"]["history"].as_array().unwrap().len(), 2);
    stop(f).await;
}

#[tokio::test]
async fn sequential_browser_refuses_unknown_or_ordinary_chat_without_another_guarded_step() {
    // A 5 s command deadline separates waiting it out from refusing on the first read, even on
    // a loaded runner.
    let limits = RunnerLimits {
        command: Duration::from_secs(5),
        ..limits()
    };
    for fault in [Fault::SequentialPrivacyUnknown, Fault::SequentialPrivacyOff] {
        let f = fixture(fault, false).await;
        let started = Instant::now();
        let error = run_within(&f, &limits).await.unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            format!("{error:#}").contains("Verified ChatGPT Temporary Chat is required"),
            "{fault:?}: {error:#}"
        );
        // Unknown waits out its command deadline; false refuses on its first read. Staging the
        // first prompt adds the editor settle wait to both.
        if matches!(fault, Fault::SequentialPrivacyUnknown) {
            assert!(
                elapsed >= limits.command,
                "{fault:?} refused after {elapsed:?}"
            );
        } else {
            assert!(
                elapsed < limits.command + crate::browser_dom::EDITOR_SETTLE,
                "{fault:?} waited {elapsed:?}"
            );
        }
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "error");
        assert_eq!(meta["recoveryRequired"], true);
        assert_eq!(meta["dispatch"]["state"], "uncertain");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert!(meta["browser"]["capturedAnswer"].is_null());
        assert!(meta["browser"]["runtime"]["completedBrowserTurns"].is_null());
        {
            let trace = f.trace.lock().unwrap();
            let send = trace
                .iter()
                .position(|r| {
                    r["method"] == "Input.dispatchKeyEvent" && r["params"]["type"] == "keyDown"
                })
                .unwrap();
            assert!(
                trace[send + 1..].iter().all(|r| !r["params"]["expression"]
                    .as_str()
                    .is_some_and(|e| e.contains("if (!oracleTemporaryChat()) throw Error("))),
                "{fault:?} ran a guarded step after an unverified read"
            );
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn sequential_browser_preserves_earlier_files_and_images_before_text_only_turns() {
    for fault in [Fault::SequentialFile, Fault::SequentialImage] {
        let f = fixture(fault, false).await;
        run(&f).await.unwrap();
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "completed");
        assert_eq!(meta["recoveryRequired"], false);
        let kind = if matches!(fault, Fault::SequentialFile) {
            "file"
        } else {
            "image"
        };
        let artifacts = meta["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            artifacts[0]["origin"]["assistantMessageId"],
            "assistant-public-1"
        );
        assert!(
            !crate::browser_output::saved_file_bytes(artifacts[0], &CancellationToken::new())
                .unwrap()
                .is_empty()
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 3);
        {
            let trace = f.trace.lock().unwrap();
            let download = trace
                .iter()
                .position(|r| {
                    r["params"]["expression"]
                        .as_str()
                        .is_some_and(|e| e.contains("industrious-dom:download-click"))
                })
                .unwrap();
            let sends = trace
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    r["method"] == "Input.dispatchKeyEvent" && r["params"]["type"] == "keyDown"
                })
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            assert!(sends[0] < download && download < sends[1]);
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn sequential_named_images_preserve_global_order_and_each_response_origin() {
    for (fault, count, first_turn) in [
        (Fault::SequentialNamedAll, 3, 1),
        (Fault::SequentialNamedFirst, 1, 1),
        (Fault::SequentialNamedLast, 1, 3),
        (Fault::SequentialNamedButtons, 3, 1),
    ] {
        let f = fixture(fault, false).await;
        run(&f).await.unwrap();
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "completed");
        assert_eq!(f.sends.load(Ordering::SeqCst), 3);
        let summary = &meta["browser"]["runtime"]["imageDelivery"];
        assert_eq!(summary["imageCount"], count);
        let images = summary["savedImages"].as_array().unwrap();
        assert_eq!(images.len(), count);
        for (i, image) in images.iter().enumerate() {
            let expected = f.home.path().join(if i == 0 {
                "PUBLIC-image.png".into()
            } else {
                format!("PUBLIC-image.{}.png", i + 1)
            });
            assert_eq!(image["path"], json!(expected));
            assert_eq!(
                image["origin"]["assistantMessageId"],
                format!("assistant-public-{}", first_turn + i)
            );
            assert_eq!(
                crate::browser_output::saved_file_bytes(image, &CancellationToken::new()).unwrap(),
                b"\x89PNG\r\n\x1a\nPUBLIC synthetic image bytes"
            );
        }
        let receipt: CaptureReceipt =
            serde_json::from_value(meta["browser"]["capturedAnswer"].clone()).unwrap();
        let answer =
            String::from_utf8(artifacts::verify_capture(&f.store, &receipt).unwrap()).unwrap();
        assert!(answer.contains("## Follow-up 2\n\n### Prompt\n\nPUBLIC third turn"));
        assert!(answer.ends_with(summary["markdownSuffix"].as_str().unwrap()));
        assert!(answer.contains(&format!("Generated {count} image(s).")));
        stop(f).await;
    }
}

#[tokio::test]
async fn sequential_named_delayed_image_recovery_reuses_published_outputs_without_another_send() {
    let f = fixture(Fault::SequentialNamedDelayed, false).await;
    let hook: CaptureHook =
        Arc::new(|_| bail!("PUBLIC interruption after required output publication"));
    let error = execute_with_capture(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &f.identity,
        &limits(),
        &CancellationToken::new(),
        Some(&hook),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("PUBLIC interruption after required output publication"),
        "{error:#}"
    );
    let saved = f.store.require(&f.id).unwrap();
    assert_eq!(saved["status"], "error");
    let output = fs::read(f.home.path().join("PUBLIC-sequence.md")).unwrap();
    assert_eq!(
        saved["browser"]["runtime"]["completedBrowserTurns"][2]["images"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let recovered = recover_existing(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &limits(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["recoveryRequired"], false);
    assert_eq!(
        fs::read(f.home.path().join("PUBLIC-sequence.md")).unwrap(),
        output
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 3);
    assert_eq!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["params"]["expression"]
                .as_str()
                .is_some_and(|e| e.contains("industrious-dom:download-click")))
            .count(),
        1
    );
    stop(f).await;
}

#[tokio::test]
async fn sequential_named_image_failure_keeps_prior_outputs_and_stops_remaining_sends() {
    for (fault, existing) in [
        (Fault::SequentialNamedBad, false),
        (Fault::SequentialNamedBadButtons, false),
        (Fault::SequentialNamedAll, true),
    ] {
        let f = fixture(fault, false).await;
        let second = f.home.path().join("PUBLIC-image.2.png");
        if existing {
            fs::write(&second, b"PUBLIC existing second destination").unwrap();
        }
        assert!(run(&f).await.is_err());
        assert_eq!(f.sends.load(Ordering::SeqCst), 2);
        assert_eq!(f.store.require(&f.id).unwrap()["status"], "error");
        assert_eq!(
            fs::read(f.home.path().join("PUBLIC-image.png")).unwrap(),
            b"\x89PNG\r\n\x1a\nPUBLIC synthetic image bytes"
        );
        if existing {
            assert_eq!(
                fs::read(&second).unwrap(),
                b"PUBLIC existing second destination"
            );
        } else {
            assert!(!second.exists());
        }
        assert!(!f.home.path().join("PUBLIC-image.3.png").exists());
        assert!(!f.home.path().join("PUBLIC-sequence.md").exists());
        stop(f).await;
    }
}

#[tokio::test]
async fn sequential_named_images_also_collect_later_file_only_turns() {
    let f = fixture(Fault::SequentialNamedFirstThenFile, false).await;
    run(&f).await.unwrap();
    let meta = f.store.require(&f.id).unwrap();
    let file = meta["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "file")
        .expect("File-only turn was omitted");
    assert_eq!(file["origin"]["assistantMessageId"], "assistant-public-2");
    assert_eq!(
        crate::browser_output::saved_file_bytes(file, &CancellationToken::new()).unwrap(),
        b"PUBLIC downloaded file bytes"
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 3);
    assert_eq!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["params"]["expression"]
                .as_str()
                .is_some_and(|e| e.contains("industrious-dom:download-click")))
            .count(),
        2
    );
    stop(f).await;
}

#[tokio::test]
async fn sequential_saved_transcript_rejects_substituted_captures_from_any_session() {
    let f = fixture(Fault::Sequential, false).await;
    run(&f).await.unwrap();
    let original = f.store.require(&f.id).unwrap();
    let foreign = f
        .store
        .create(
            json!({"prompt":"PUBLIC unrelated turn capture"}),
            f.home.path(),
            Some("public-foreign"),
        )
        .unwrap();
    for id in [f.id.as_str(), foreign["id"].as_str().unwrap()] {
        let capture = artifacts::capture(
            &f.store,
            id,
            "public-unrelated.md",
            b"PUBLIC unrelated answer",
            &CancellationToken::new(),
        )
        .unwrap();
        let mut meta = original.clone();
        meta["browser"]["runtime"]["completedBrowserTurns"][0]["capture"] = json!(capture);
        assert!(crate::browser_turns::saved_transcript(&f.store, &meta).is_err());
        if id != f.id {
            meta["dispatch"]["history"][0]["capture"] = json!(capture);
            assert!(crate::browser_turns::saved_transcript(&f.store, &meta).is_err());
        }
    }
    stop(f).await;
}

#[tokio::test]
async fn sequential_recovery_refuses_unsent_turns_and_recovers_the_complete_final_transcript() {
    for fault in [Fault::SequentialLostAck, Fault::SequentialLastLostAck] {
        let f = fixture(fault, false).await;
        let error = run(&f).await.unwrap_err();
        let saved = f.store.require(&f.id).unwrap();
        assert_eq!(saved["status"], "error", "{fault:?}: {error:#}");
        assert_eq!(saved["recoveryRequired"], true, "{fault:?}: {error:#}");
        assert_eq!(
            f.sends.load(Ordering::SeqCst),
            if matches!(fault, Fault::SequentialLostAck) {
                2
            } else {
                3
            },
            "{fault:?}: {error:#}"
        );
        if matches!(fault, Fault::SequentialLostAck) {
            let before = fs::read(f.store.directory(&f.id).unwrap().join("meta.json")).unwrap();
            let error = recover_existing(
                &f.store,
                &f.id,
                f.profile.clone(),
                f.cdp.clone(),
                f.endpoint.clone(),
                &limits(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("before all follow-ups were dispatched"),
                "{error:#}"
            );
            assert_eq!(
                fs::read(f.store.directory(&f.id).unwrap().join("meta.json")).unwrap(),
                before
            );
            assert_eq!(f.sends.load(Ordering::SeqCst), 2);
            stop(f).await;
            continue;
        }
        let (client, server) = tokio::io::duplex(1024 * 1024);
        let recovery = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut turns = vec![request(&f.store, &f.id).unwrap().prompt];
        turns.extend(crate::browser_turns::prompts(&saved["options"]).unwrap());
        let trace = Arc::new(Mutex::new(vec![]));
        let peer = tokio::spawn(
            Exchange {
                fault: Fault::Sequential,
                prompt: turns.last().unwrap().clone(),
                turns,
                url: "https://chatgpt.com/c/consult-public".into(),
                owner: saved["browser"]["runtime"]["submissionOwner"].clone(),
                commit: Value::Null,
                before: false,
                sends: f.sends.clone(),
                closes: f.closes.clone(),
                closed: false,
                trace: trace.clone(),
                cancel: CancellationToken::new(),
                store: f.store.clone(),
                id: f.id.clone(),
                download_stage: None,
                result_reads: 0,
                recovery_observed: false,
                download_count: 0,
                delayed_image_visible: false,
                retirement_marker: None,
                pending_retirement_read: false,
                cleanup_lock: None,
            }
            .run(server),
        );
        let result = recover_existing(
            &f.store,
            &f.id,
            f.profile.clone(),
            recovery.clone(),
            f.endpoint.clone(),
            &limits(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["recoveryRequired"], false);
        assert_eq!(f.sends.load(Ordering::SeqCst), 3);
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        assert_eq!(
            String::from_utf8(captured(&f.store, &f.id).unwrap()).unwrap(),
            crate::browser_turns::saved_transcript(&f.store, &result).unwrap()
        );
        assert!(trace.lock().unwrap().iter().all(|r| !matches!(
            r["method"].as_str(),
            Some("Target.createTarget" | "Input.dispatchKeyEvent" | "DOM.setFileInputFiles")
        )));
        recovery
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
        stop(f).await;
    }
}

#[tokio::test]
async fn delivery_refuses_replaced_target_options_and_terminal_outcomes_after_download() {
    for fault in [
        Fault::TargetDuringDownload,
        Fault::ModelDuringDownload,
        Fault::PartialDuringDownload,
        Fault::CancelledDuringDownload,
        Fault::OptionsDuringDownload,
        Fault::CancelDuringDownload,
    ] {
        let f = fixture(fault, false).await;
        assert!(run(&f).await.is_err(), "{fault:?}");
        let meta = f.store.require(&f.id).unwrap();
        assert_ne!(meta["status"], "completed");
        assert!(!f.home.path().join("PUBLIC-image.png").exists());
        assert!(!f.home.path().join("PUBLIC-output.md").exists());
        assert!(!f.home.path().join("PUBLIC-foreign-output.md").exists());
        assert!(
            meta["artifacts"]
                .as_array()
                .is_none_or(|rows| rows.iter().all(|row| row["kind"] != "image"))
        );
        assert!(
            !f.store
                .log(&f.id)
                .unwrap()
                .contains("PUBLIC exact owned answer")
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            usize::from(f.cancel.is_cancelled()),
            "{fault:?}"
        );
        if matches!(
            fault,
            Fault::PartialDuringDownload | Fault::CancelledDuringDownload
        ) {
            assert_eq!(
                meta["status"],
                if matches!(fault, Fault::PartialDuringDownload) {
                    "partial"
                } else {
                    "cancelled"
                }
            );
            assert_eq!(meta["completedAt"], "PUBLIC winning terminal time");
        }
        if matches!(fault, Fault::ModelDuringDownload) {
            assert_eq!(meta["models"][0]["marker"], "PUBLIC winning model outcome");
        }
        if matches!(fault, Fault::CancelDuringDownload) {
            assert_eq!(meta["browser"]["runtime"]["downloadContextSettled"], true);
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn completion_preserves_receipt_browser_and_dispatch_updates_during_download_cleanup() {
    let f = fixture(Fault::NewerReceipt, false).await;
    let result = run(&f).await.unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["artifacts"][0]["extension"], "PUBLIC newer receipt");
    assert_eq!(
        result["browser"]["harvest"]["marker"],
        "PUBLIC newer harvest"
    );
    assert_eq!(result["dispatch"]["marker"], "PUBLIC newer dispatch");
    stop(f).await;
}

#[tokio::test]
async fn recovery_refuses_terminal_model_or_cancelled_session_before_changing_any_record() {
    for cancelled in [false, true] {
        let f = fixture(Fault::LostSendAck, false).await;
        assert!(run(&f).await.is_err());
        if cancelled {
            f.store
                .update(&f.id, |_| {
                    Ok(Some(
                        json!({"status":"cancelled","completedAt":"PUBLIC winning terminal time"}),
                    ))
                })
                .unwrap();
        }
        let model = f.store.require(&f.id).unwrap()["model"]
            .as_str()
            .unwrap()
            .to_owned();
        f.store.update_model(&f.id,&model,&json!({"status":if cancelled{"cancelled"}else{"partial"},"marker":"PUBLIC winning terminal model"})).unwrap();
        let dir = f.store.directory(&f.id).unwrap();
        let metadata = fs::read(dir.join("meta.json")).unwrap();
        let model_path = dir.join(format!("models/{}.json", sessions::safe_model_slug(&model)));
        let model_bytes = fs::read(&model_path).unwrap();
        assert!(
            recover_existing(
                &f.store,
                &f.id,
                f.profile.clone(),
                f.cdp.clone(),
                f.endpoint.clone(),
                &limits(),
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read(dir.join("meta.json")).unwrap(), metadata);
        assert_eq!(fs::read(model_path).unwrap(), model_bytes);
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        stop(f).await;
    }
}

#[tokio::test]
async fn completed_recovery_only_retries_cleanup_and_preserves_committed_results_on_failure() {
    for fault in [
        Fault::CleanupRetry,
        Fault::CleanupCleanRetry,
        Fault::ForeignRetirement,
    ] {
        let f = fixture(fault, false).await;
        let before = run(&f).await.unwrap();
        assert_eq!(before["status"], "completed");
        assert_eq!(before["recoveryRequired"], true);
        let model = before["model"].as_str().unwrap();
        let model_bytes = fs::read(
            f.store
                .directory(&f.id)
                .unwrap()
                .join(format!("models/{}.json", sessions::safe_model_slug(model))),
        )
        .unwrap();
        let answer = captured(&f.store, &f.id).unwrap();
        let output = fs::read(f.home.path().join("PUBLIC-output.md")).unwrap();
        let log = f.store.log(&f.id).unwrap();
        let recovered = recover_existing(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &limits(),
            &f.cancel,
        )
        .await;
        assert_eq!(
            recovered.is_ok(),
            matches!(fault, Fault::CleanupRetry | Fault::CleanupCleanRetry)
        );
        let after = f.store.require(&f.id).unwrap();
        assert_eq!(after["status"], "completed");
        assert_eq!(after["completedAt"], before["completedAt"]);
        assert_eq!(after["capturedAnswer"], before["capturedAnswer"]);
        assert_eq!(after["artifacts"], before["artifacts"]);
        assert_eq!(
            after["recoveryRequired"],
            !matches!(fault, Fault::CleanupRetry | Fault::CleanupCleanRetry)
        );
        assert_eq!(captured(&f.store, &f.id).unwrap(), answer);
        assert_eq!(
            fs::read(f.home.path().join("PUBLIC-output.md")).unwrap(),
            output
        );
        assert_eq!(f.store.log(&f.id).unwrap(), log);
        assert_eq!(
            fs::read(
                f.store
                    .directory(&f.id)
                    .unwrap()
                    .join(format!("models/{}.json", sessions::safe_model_slug(model)))
            )
            .unwrap(),
            model_bytes
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            usize::from(matches!(
                fault,
                Fault::CleanupRetry | Fault::CleanupCleanRetry
            ))
        );
        stop(f).await;
    }
}

#[tokio::test]
async fn changed_completed_content_after_retirement_marker_preserves_target_and_result() {
    for fault in [
        Fault::AnswerDuringRetirement,
        Fault::SourceDuringRetirement,
        Fault::RecoveryAnswerDuringRetirement,
        Fault::RecoverySourceDuringRetirement,
    ] {
        let f = fixture(fault, false).await;
        let before = run(&f).await.unwrap();
        assert_eq!(before["status"], "completed");
        assert_eq!(before["recoveryRequired"], true);
        let answer = captured(&f.store, &f.id).unwrap();
        if matches!(
            fault,
            Fault::RecoveryAnswerDuringRetirement | Fault::RecoverySourceDuringRetirement
        ) {
            assert!(
                recover_existing(
                    &f.store,
                    &f.id,
                    f.profile.clone(),
                    f.cdp.clone(),
                    f.endpoint.clone(),
                    &limits(),
                    &f.cancel
                )
                .await
                .is_err()
            );
        }
        let after = f.store.require(&f.id).unwrap();
        assert_eq!(after["status"], "completed");
        assert_eq!(after["completedAt"], before["completedAt"]);
        assert_eq!(captured(&f.store, &f.id).unwrap(), answer);
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        {
            let trace = f.trace.lock().unwrap();
            assert!(trace.iter().any(|call| {
                call["params"]["expression"]
                    .as_str()
                    .is_some_and(|s| s.contains("industrious-dom:retirement-rollback"))
            }));
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn changed_or_missing_saved_image_refuses_completed_recovery_retirement() {
    for remove in [false, true] {
        let f = fixture(Fault::CleanupRetry, false).await;
        let before = run(&f).await.unwrap();
        let image = f.home.path().join("PUBLIC-required-image.png");
        let bytes = b"PUBLIC saved image bytes";
        fs::write(&image, bytes).unwrap();
        f.store.update(&f.id,|current| {let mut artifacts=current["artifacts"].as_array().unwrap().clone();artifacts.push(json!({"kind":"image","path":image,"sizeBytes":bytes.len(),"sha256":sha256(bytes)}));Ok(Some(json!({"artifacts":artifacts})))}).unwrap();
        let journal = TargetJournal::open(&f.store).unwrap();
        let creator = &before["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
        let record = journal.read(&creator["allocationId"]).unwrap();
        if remove {
            fs::remove_file(&image).unwrap();
        } else {
            fs::write(&image, b"PUBLIC changed image bytes").unwrap();
        }
        let answer = captured(&f.store, &f.id).unwrap();
        assert!(
            recover_existing(
                &f.store,
                &f.id,
                f.profile.clone(),
                f.cdp.clone(),
                f.endpoint.clone(),
                &limits(),
                &f.cancel
            )
            .await
            .is_err()
        );
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), record);
        let after = f.store.require(&f.id).unwrap();
        assert_eq!(after["status"], "completed");
        assert_eq!(after["completedAt"], before["completedAt"]);
        assert_eq!(after["recoveryRequired"], true);
        assert_eq!(captured(&f.store, &f.id).unwrap(), answer);
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        stop(f).await;
    }
}

#[tokio::test]
async fn uncertain_close_preserves_the_retirement_marker_and_admission() {
    let f = fixture(Fault::UncertainClose, false).await;
    let completed = run(&f).await.unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["recoveryRequired"], true);
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    {
        let trace = f.trace.lock().unwrap();
        assert!(!trace.iter().any(|call| {
            call["params"]["expression"]
                .as_str()
                .is_some_and(|s| s.contains("industrious-dom:retirement-rollback"))
        }));
    }
    let leases = f.profile.records().unwrap();
    assert!(
        leases["leases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|lease| lease["chromeTargetId"] == "TARGET-PUBLIC"
                && lease["targetDispositionPending"] == true)
    );
    stop(f).await;
}

#[tokio::test]
async fn completed_tabs_retry_a_transient_close_without_waiting_for_another_request() {
    let f = fixture(Fault::CloseRefusedOnce, false).await;
    let completed = run(&f).await.unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["recoveryRequired"], false);
    assert!(completed["cleanupWarning"].is_null());
    assert_eq!(
        completed["browser"]["runtime"]["targetDispositionPending"],
        false
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 2);
    assert!(
        f.profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let journal = TargetJournal::open(&f.store).unwrap();
    let creator = &completed["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let record = journal.read(&creator["allocationId"]).unwrap();
    assert_eq!(record["disposition"]["absence"], "confirmed-absent");
    assert!(!captured(&f.store, &f.id).unwrap().is_empty());
    stop(f).await;
}

#[tokio::test]
async fn a_lock_held_through_cleanup_and_warning_keeps_the_completed_answer() {
    let f = fixture(Fault::CloseRefusedThenLockedSettlement, false).await;
    let started = std::time::Instant::now();
    let result = run_within(&f, &one_second_command_limits()).await;
    // Bounded well below the 60 s defaults; about 12 s unloaded, more on a 2-core CI runner.
    assert!(started.elapsed() < Duration::from_secs(30));
    let completed =
        result.expect("Cleanup warning persistence must not replace a completed answer");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["dispatch"]["state"], "completed");
    assert_eq!(completed["recoveryRequired"], true);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 2);
    assert!(!captured(&f.store, &f.id).unwrap().is_empty());
    let journal = TargetJournal::open(&f.store).unwrap();
    let creator = &completed["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let record = journal.read(&creator["allocationId"]).unwrap();
    assert_eq!(record["disposition"]["absence"], "confirmed-absent");
    stop(f).await;
}

#[tokio::test]
async fn completed_close_retries_resume_only_the_exact_durable_marker() {
    for explicit in [false, true] {
        for fault in [
            Fault::CloseRefusedTwice,
            Fault::CloseRefusedForeignMarker,
            Fault::CloseRefusedChangedSource,
            Fault::CloseRefusedActiveThinking,
            Fault::CloseRefusedMissingCompletion,
        ] {
            let f = fixture(fault, false).await;
            let completed = run(&f).await.unwrap();
            assert_eq!(completed["status"], "completed");
            assert_eq!(completed["recoveryRequired"], true);
            assert_eq!(
                f.closes.load(Ordering::SeqCst),
                if matches!(fault, Fault::CloseRefusedTwice) {
                    2
                } else {
                    1
                }
            );
            let answer = captured(&f.store, &f.id).unwrap();
            let journal = TargetJournal::open(&f.store).unwrap();
            let creator = &completed["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
            let record = journal.read(&creator["allocationId"]).unwrap();
            let marker = journal.retirement_id(creator, &record["commit"]).unwrap();
            if explicit {
                let result = recover_existing(
                    &f.store,
                    &f.id,
                    f.profile.clone(),
                    f.cdp.clone(),
                    f.endpoint.clone(),
                    &limits(),
                    &f.cancel,
                )
                .await;
                assert_eq!(
                    result.is_ok(),
                    matches!(fault, Fault::CloseRefusedTwice),
                    "{fault:?}: {result:?}"
                );
            } else {
                crate::browser_retirement::reconcile(&f.store, &f.cdp, &f.endpoint)
                    .await
                    .unwrap();
            }
            let after = journal.read(&creator["allocationId"]).unwrap();
            assert_eq!(after["retirement"]["id"], marker);
            let closed = matches!(fault, Fault::CloseRefusedTwice);
            assert_eq!(
                after["disposition"]["decision"],
                if closed { "retired" } else { "retained" }
            );
            assert_eq!(f.closes.load(Ordering::SeqCst), if closed { 3 } else { 1 });
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
            assert_eq!(captured(&f.store, &f.id).unwrap(), answer);
            assert_eq!(
                f.store.require(&f.id).unwrap()["completedAt"],
                completed["completedAt"]
            );
            assert_eq!(
                f.profile.records().unwrap()["leases"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                closed
            );
            stop(f).await;
        }
    }
}

#[tokio::test]
async fn same_process_reconciliation_never_closes_running_or_uncertain_requests() {
    for case in [
        "running",
        "send-uncertain",
        "completed-uncertain",
        "missing-dispatch",
        "execution-reservation",
        "submission-owner",
    ] {
        let f = fixture(
            if case == "send-uncertain" {
                Fault::LostSendAck
            } else {
                Fault::CloseRefusedTwice
            },
            false,
        )
        .await;
        if case == "send-uncertain" {
            assert!(run(&f).await.is_err());
        } else {
            run(&f).await.unwrap();
            f.store
                .update(&f.id, |current| {
                    let mut changed = current.clone();
                    match case {
                        "running" => changed["status"] = json!("running"),
                        "completed-uncertain" => changed["dispatch"]["state"] = json!("uncertain"),
                        "missing-dispatch" => changed["dispatch"] = Value::Null,
                        "execution-reservation" => {
                            changed["dispatch"]["reservationId"] = json!(Uuid::new_v4().to_string())
                        }
                        "submission-owner" => {
                            changed["dispatch"]["submissionOwner"]["reservationId"] =
                                json!(Uuid::new_v4().to_string())
                        }
                        _ => unreachable!(),
                    }
                    Ok(Some(changed))
                })
                .unwrap();
        }
        let closes = f.closes.load(Ordering::SeqCst);
        crate::browser_retirement::reconcile(&f.store, &f.cdp, &f.endpoint)
            .await
            .unwrap();
        assert_eq!(f.closes.load(Ordering::SeqCst), closes);
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        stop(f).await;
    }
}

#[tokio::test]
async fn required_file_exports_are_committed_before_retirement_and_need_an_answer_path() {
    for fault in [Fault::FileExport, Fault::FileExportNoAnswer] {
        let f = fixture(fault, true).await;
        let collision = f.home.path().join("public.txt");
        fs::write(&collision, b"PUBLIC existing output").unwrap();
        let result = run(&f).await;
        let current = f.store.require(&f.id).unwrap();
        assert_eq!(result.is_ok(), matches!(fault, Fault::FileExport));
        assert_eq!(fs::read(&collision).unwrap(), b"PUBLIC existing output");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            usize::from(matches!(fault, Fault::FileExport))
        );
        if matches!(fault, Fault::FileExport) {
            let exported = current["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|artifact| {
                    artifact["label"]
                        .as_str()
                        .is_some_and(|label| label.ends_with(" (write-output)"))
                })
                .unwrap();
            assert_eq!(
                fs::read(exported["path"].as_str().unwrap()).unwrap(),
                b"PUBLIC downloaded file bytes"
            );
            assert_eq!(
                Path::new(exported["path"].as_str().unwrap())
                    .parent()
                    .unwrap(),
                crate::native_fs::canonical_wire_path(f.home.path()).unwrap()
            );
            assert!(exported["path"].as_str().unwrap().ends_with("public-2.txt"));
            let log = f.store.log(&f.id).unwrap();
            let saved = format!(
                "Saved assistant output to {}\n",
                current["browser"]["requiredTextOutput"]["file"]
                    .as_str()
                    .unwrap()
            );
            let copied = format!(
                "[browser] Saved write-output file artifact to {} sha256={}\n",
                exported["path"].as_str().unwrap(),
                exported["sha256"].as_str().unwrap()
            );
            assert!(
                log.find(&saved).unwrap() < log.find(&copied).unwrap(),
                "{log}"
            );
            let journal = TargetJournal::open(&f.store).unwrap();
            let creator = &current["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
            let record = journal.read(&creator["allocationId"]).unwrap();
            assert!(
                record["commit"]["requiredOutputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|receipt| receipt["file"] == exported["path"]
                        && receipt["sha256"] == exported["sha256"])
            );
            assert_eq!(record["disposition"]["decision"], "retired");
        } else {
            assert_eq!(current["recoveryRequired"], true);
            assert!(
                current["errorMessage"]
                    .as_str()
                    .unwrap()
                    .contains("saved answer path")
            );
        }
        stop(f).await;
    }
}

#[test]
fn url_followups_are_rejected_before_session_storage_or_target_allocation() {
    let home = tempfile::tempdir().unwrap();
    for fault in [
        Fault::Followup,
        Fault::LegacyFollowup,
        Fault::FollowupExtraHigh,
        Fault::FollowupWrongModel,
        Fault::FollowupWrongTier,
    ] {
        let store = SessionStore::new(home.path().join(".oracle"));
        let mut request = crate::execution::resolve(
            &json!({"prompt":"PUBLIC follow-up input","engine":"browser"}),
            &json!({}),
            &Environment::new(),
            home.path(),
        )
        .unwrap();
        request.browser["resumeConversationUrl"] =
            json!(if matches!(fault, Fault::LegacyFollowup) {
                "https://chat.openai.com/c/public"
            } else {
                "https://chatgpt.com/c/public"
            });
        if matches!(fault, Fault::FollowupExtraHigh | Fault::FollowupWrongTier) {
            request.browser["thinkingTime"] = json!("extra-high");
        }
        if matches!(fault, Fault::FollowupWrongModel) {
            request.browser["desiredModel"] = json!("GPT-5.6 Sol");
        }
        let error = prepare(&store, &request, home.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Temporary Chat cannot be reopened by URL")
        );
        assert!(!store.root().exists());
    }
    for url in [
        "https://chatgpt.com/c/public",
        "https://chat.openai.com/c/public",
        "https://external.invalid/c/public",
        "http://chatgpt.com/c/public",
        "https://chatgpt.com/",
        "https://chatgpt.com:444/c/public",
        "https://user@chatgpt.com/c/public",
    ] {
        let store = SessionStore::new(home.path().join(".oracle"));
        let mut request = crate::execution::resolve(
            &json!({"prompt":"PUBLIC follow-up input","engine":"browser"}),
            &json!({}),
            &Environment::new(),
            home.path(),
        )
        .unwrap();
        request.browser["resumeConversationUrl"] = json!(url);
        request.options["browserConfig"] = request.browser.clone();
        assert!(prepare(&store, &request, home.path()).is_err(), "{url}");
        assert!(!store.root().exists());
    }
    for (model, effort) in [("GPT-5.6 Sol", "pro"), ("GPT-6", "extra-high")] {
        let store = SessionStore::new(home.path().join(".oracle"));
        let mut request = crate::execution::resolve(
            &json!({"prompt":"PUBLIC fresh browser request","engine":"browser"}),
            &json!({}),
            &Environment::new(),
            home.path(),
        )
        .unwrap();
        request.browser["desiredModel"] = json!(model);
        request.browser["thinkingTime"] = json!(effort);
        assert!(prepare(&store, &request, home.path()).is_err());
        assert!(!store.root().exists());
    }
}

#[tokio::test]
async fn failed_required_file_export_reuses_owned_answer_and_captured_file_without_replay() {
    for fault in [Fault::FileExportChangedSource, Fault::FileExportFormatted] {
        let f = fixture(fault, true).await;
        assert!(run(&f).await.is_err());
        let before = f.store.require(&f.id).unwrap();
        assert_eq!(before["recoveryRequired"], true);
        let answer = f.home.path().join("PUBLIC-output.md");
        if matches!(fault, Fault::FileExportChangedSource) {
            let message = before["errorMessage"].as_str().unwrap();
            assert!(
                message.starts_with("Failed to copy saved file artifact ")
                    && message.contains(&format!(
                        " beside {}: ",
                        crate::native_fs::canonical_wire_path(f.home.path())
                            .unwrap()
                            .join("PUBLIC-output.md")
                            .display()
                    )),
                "{message}"
            );
        }
        let output = fs::read(&answer).unwrap();
        if matches!(fault, Fault::FileExportFormatted) {
            assert!(String::from_utf8_lossy(&output).contains("\n\n**PUBLIC exact owned answer**"));
        }
        let native_before = crate::native_fs::NativeEntry::open(&answer, false)
            .unwrap()
            .identity;
        let file = before["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|artifact| artifact["kind"] == "file")
            .unwrap();
        fs::write(
            file["path"].as_str().unwrap(),
            b"PUBLIC downloaded file bytes",
        )
        .unwrap();
        let recovered = recover_existing(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &limits(),
            &f.cancel,
        )
        .await
        .unwrap();
        assert_eq!(recovered["status"], "completed");
        assert_eq!(recovered["recoveryRequired"], false);
        assert_eq!(
            recovered["browser"]["requiredTextOutput"],
            before["browser"]["requiredTextOutput"]
        );
        assert_eq!(fs::read(&answer).unwrap(), output);
        assert_eq!(
            crate::native_fs::NativeEntry::open(&answer, false)
                .unwrap()
                .identity,
            native_before
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        {
            let trace = f.trace.lock().unwrap();
            assert_eq!(
                trace
                    .iter()
                    .filter(|request| {
                        request["params"]["expression"]
                            .as_str()
                            .is_some_and(|expression| {
                                expression.contains("industrious-dom:download-click")
                            })
                    })
                    .count(),
                1
            );
        }
        stop(f).await;
    }
}

#[test]
fn saved_files_distinguish_download_buttons_and_preserve_direct_url_identity() {
    let observed =
        json!({"lastAssistantMessageId":"PUBLIC assistant","lastAssistantTurnId":"PUBLIC turn"});
    let candidate =
        json!({"url":"browser-download","candidateId":"1:0","filename":"PUBLIC first.csv"});
    let mut saved = json!({"kind":"file","sourceUrl":"browser-download","candidateId":"1:0","filename":"PUBLIC first.csv","origin":{"targetId":"PUBLIC target","assistantMessageId":"PUBLIC assistant","assistantTurnId":"PUBLIC turn"}});
    assert!(matches_saved_file(
        &saved,
        &candidate,
        "PUBLIC target",
        &observed
    ));
    let second =
        json!({"url":"browser-download","candidateId":"1:1","filename":"PUBLIC second.csv"});
    assert!(!matches_saved_file(
        &saved,
        &second,
        "PUBLIC target",
        &observed
    ));
    let changed_name =
        json!({"url":"browser-download","candidateId":"1:0","filename":"PUBLIC renamed.csv"});
    assert!(!matches_saved_file(
        &saved,
        &changed_name,
        "PUBLIC target",
        &observed
    ));
    saved["sourceUrl"] = json!("https://chatgpt.com/backend-api/files/file_PUBLIC/download");
    let direct = json!({"url":saved["sourceUrl"],"candidateId":"","filename":"PUBLIC.csv"});
    assert!(matches_saved_file(
        &saved,
        &direct,
        "PUBLIC target",
        &observed
    ));
    assert!(!matches_saved_file(
        &saved,
        &direct,
        "PUBLIC foreign target",
        &observed
    ));
}

#[tokio::test]
async fn partial_two_button_recovery_downloads_the_uncaptured_control_and_exports_both() {
    let f = fixture(Fault::TwoFilePartial, true).await;
    let options = f.store.require(&f.id).unwrap()["options"].clone();
    assert!(run(&f).await.is_err());
    let before = f.store.require(&f.id).unwrap();
    assert_eq!(before["browser"]["runtime"]["downloadContextSettled"], true);
    assert_eq!(
        before["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "file")
            .count(),
        1
    );
    f.store
        .update(&f.id, |_| Ok(Some(json!({"options":options}))))
        .unwrap();
    let recovered = recover_existing(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &limits(),
        &f.cancel,
    )
    .await
    .unwrap();
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["recoveryRequired"], false);
    let files = recovered["artifacts"].as_array().unwrap();
    assert_eq!(
        files
            .iter()
            .filter(|row| row["candidateId"].as_str().is_some_and(|id| !id.is_empty()))
            .count(),
        2
    );
    assert_eq!(
        files
            .iter()
            .filter(|row| row["label"]
                .as_str()
                .is_some_and(|label| label.ends_with(" (write-output)")))
            .count(),
        2
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    {
        let trace = f.trace.lock().unwrap();
        assert_eq!(
            trace
                .iter()
                .filter(|request| {
                    request["params"]["expression"]
                        .as_str()
                        .is_some_and(|expression| {
                            expression.contains("industrious-dom:download-click")
                        })
                })
                .count(),
            3
        );
    }
    stop(f).await;
}

#[tokio::test]
async fn renderer_reservation_refuses_reusing_or_repreparing_a_physical_send() {
    let f = fixture(Fault::None, false).await;
    let timeout = Duration::from_secs(1);
    f.cdp
        .call(
            "Page.navigate",
            json!({"url":"https://chatgpt.com/"}),
            timeout,
            &f.cancel,
        )
        .await
        .unwrap();
    let mut reservation =
        RendererReservation::acquire(f.cdp.clone(), "claim-public".into(), timeout, &f.cancel)
            .await
            .unwrap();
    let (_, nav) = browser_dom::capture_navigation(&reservation, timeout, &f.cancel)
        .await
        .unwrap();
    let prompt = "PUBLIC single-use physical send contract";
    let prepared = PreparedPrompt::stage(&mut reservation, prompt, &nav, &[], timeout, &f.cancel)
        .await
        .unwrap();
    prepared
        .arm_send(&mut reservation, timeout, &f.cancel)
        .await
        .unwrap();
    reservation
        .dispatch_started(timeout, &f.cancel)
        .await
        .unwrap();
    prepared
        .send_once(&mut reservation, timeout, &f.cancel)
        .await
        .unwrap();
    assert!(
        prepared
            .send_once(&mut reservation, timeout, &f.cancel)
            .await
            .is_err()
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    let (_, nav) = browser_dom::capture_navigation(&reservation, timeout, &f.cancel)
        .await
        .unwrap();
    let fresh = PreparedPrompt::stage(&mut reservation, prompt, &nav, &[], timeout, &f.cancel)
        .await
        .unwrap();
    fresh
        .arm_send(&mut reservation, timeout, &f.cancel)
        .await
        .unwrap();
    assert!(
        fresh
            .send_once(&mut reservation, timeout, &f.cancel)
            .await
            .is_err()
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    fresh.cleanup(&mut reservation, timeout).await.unwrap();
    stop(f).await;
}

#[tokio::test]
async fn complete_sequence_commits_full_source_captures_exact_answer_and_retires_only_owned_target()
{
    for fault in [Fault::None, Fault::DisabledPro, Fault::ProModel] {
        let f = fixture(fault, false).await;
        let result = run(&f).await.unwrap();
        assert_eq!(result["status"], "completed");
        let saved = crate::followup::browser_parent(&f.id, &result)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved["thinkingTime"],
            if matches!(fault, Fault::DisabledPro) {
                "extra-high"
            } else {
                "pro"
            }
        );
        assert_eq!(result["recoveryRequired"], false);
        assert_eq!(result["dispatch"]["state"], "completed");
        assert_eq!(
            captured(&f.store, &f.id).unwrap(),
            b"PUBLIC exact owned answer\n\nEnd\n"
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        assert!(
            f.profile.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            result["browser"]["runtime"]["thinkingSelection"]["requestedLevel"],
            if matches!(fault, Fault::DisabledPro) {
                "extra-high"
            } else {
                "pro"
            }
        );
        // Node ensureThinkingTime evidence survives the later verification.
        let thinking = &result["browser"]["runtime"]["thinkingSelection"];
        assert_eq!(thinking["status"], "switched");
        assert_eq!(thinking["targetModelKind"], Value::Null);
        assert!(thinking.get("targetModelKind").is_some());
        assert_eq!(
            thinking["observedModelKind"],
            if matches!(fault, Fault::DisabledPro) {
                json!("thinking")
            } else {
                Value::Null
            }
        );
        assert_eq!(
            thinking["observedModelLabel"],
            if matches!(fault, Fault::ProModel) {
                "Pro"
            } else {
                "GPT-6"
            }
        );
        let trace = f.trace.lock().unwrap().clone();
        let pro = trace
            .iter()
            .position(|v| {
                v["params"]["expression"].as_str().is_some_and(|s| {
                    s.contains("industrious-dom:thinking") && s.contains("\"level\":\"pro\"")
                })
            })
            .unwrap();
        if matches!(fault, Fault::DisabledPro) {
            let extra = trace
                .iter()
                .position(|v| {
                    v["params"]["expression"].as_str().is_some_and(|s| {
                        s.contains("industrious-dom:thinking")
                            && s.contains("\"level\":\"extra-high\"")
                    })
                })
                .unwrap();
            assert!(pro < extra);
        }
        assert!(run(&f).await.is_err());
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        stop(f).await;
    }
}
#[tokio::test]
async fn completed_runs_save_the_reference_transcript_artifact() {
    let f = fixture(Fault::None, false).await;
    let result = run(&f).await.unwrap();
    assert_eq!(result["status"], "completed");
    let transcripts = result["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|artifact| artifact["kind"] == "transcript")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(transcripts.len(), 1);
    let artifact = &transcripts[0];
    assert_eq!(artifact["label"], "Browser transcript");
    assert_eq!(artifact["mimeType"], "text/markdown");
    assert_eq!(artifact["validation"], json!({"type":"generic","ok":true}));
    let path = std::path::Path::new(artifact["path"].as_str().unwrap());
    assert_eq!(path.file_name().unwrap(), "transcript.md");
    let body = String::from_utf8(fs::read(path).unwrap()).unwrap();
    assert_eq!(artifact["sha256"], sha256(body.as_bytes()));
    let prompt = request(&f.store, &f.id).unwrap().prompt;
    let heading = crate::browser_output::answer_heading(&f.store.require(&f.id).unwrap());
    let url = result["browser"]["runtime"]["acceptedObservation"]["currentUrl"]
        .as_str()
        .unwrap();
    assert_eq!(
        body,
        format!(
            "# Oracle Browser Transcript\n\nConversation: {url}\n\n## Prompt\n\n{}\n\n## {heading}\n\nPUBLIC exact owned answer\n\nEnd\n",
            prompt.trim()
        )
    );
    // Reference session log after capture: evidence, labelled answer and finish line.
    let log = f.store.log(&f.id).unwrap();
    let evidence = log
        .find("[browser] Model selection evidence: requestedKey=")
        .unwrap();
    let thinking = log
        .find("\n[browser] Thinking effort evidence: requestedLevel=")
        .unwrap();
    let answer = log
        .find(&format!(
            "\n{heading}\nPUBLIC exact owned answer\n\nEnd\n\n"
        ))
        .unwrap();
    assert!(evidence < thinking && thinking < answer);
    let finish = log.lines().last().unwrap();
    assert!(finish.contains("[browser] \u{b7} \u{2191}"), "{finish}");
    // Reference archive decision: Temporary Chat is never archived.
    let archive = log
        .find("[browser] ChatGPT archive skipped (temporary-chat).\n")
        .unwrap();
    assert!(archive < evidence);
    let tab = &result["browser"]["runtime"]["tabUrl"];
    assert!(tab.is_string());
    assert_eq!(
        f.store.require(&f.id).unwrap()["browser"]["archive"],
        json!({"mode":"auto","attempted":false,"archived":false,"reason":"temporary-chat","conversationUrl":tab})
    );
    stop(f).await;
}
#[tokio::test]
async fn durable_upload_survives_original_removal_and_retires_after_capture() {
    let f = fixture(Fault::None, true).await;
    let saved = request(&f.store, &f.id).unwrap();
    let original =
        std::path::PathBuf::from(f.store.require(&f.id).unwrap()["cwd"].as_str().unwrap())
            .join("public.txt");
    fs::remove_file(original).unwrap();
    let result = run(&f).await.unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert!(!saved.staged_uploads.unwrap().directory.exists());
    stop(f).await;
}
#[tokio::test]
async fn modified_durable_upload_refuses_before_target_creation_or_send() {
    let f = fixture(Fault::None, true).await;
    let saved = request(&f.store, &f.id).unwrap();
    fs::write(&saved.upload_paths[0], b"PUBLIC changed file").unwrap();
    assert!(run(&f).await.is_err());
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert!(f.trace.lock().unwrap().is_empty());
    assert!(saved.upload_paths[0].exists());
    stop(f).await;
}
#[tokio::test]
async fn truncated_prompt_refuses_before_send_without_a_fallback() {
    let f = fixture(Fault::TruncatedComposer, false).await;
    let initial = request(&f.store, &f.id).unwrap();
    assert!(initial.fallback.is_none() && initial.prompt.len() > 50_000);
    assert!(run(&f).await.is_err());
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert!(!request(&f.store, &f.id).unwrap().fallback_used);
    assert_ne!(f.store.require(&f.id).unwrap()["status"], "completed");
    stop(f).await;
}
#[tokio::test]
async fn small_mismatch_and_uncertain_prompt_cleanup_never_authorise_fallback() {
    for fault in [
        Fault::SmallComposerMismatch,
        Fault::PromptCleanupFailed,
        Fault::PromptRollbackFailed,
    ] {
        let f = fixture(fault, false).await;
        assert!(run(&f).await.is_err());
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        assert!(!request(&f.store, &f.id).unwrap().fallback_used);
        assert_eq!(
            f.trace
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event["method"] == "DOM.setFileInputFiles")
                .count(),
            0
        );
        assert_ne!(f.store.require(&f.id).unwrap()["status"], "completed");
        stop(f).await;
    }
}
#[tokio::test]
async fn wrong_model_tier_generic_unavailability_and_unclean_inspection_refuse_before_send() {
    for fault in [
        Fault::WrongModel,
        Fault::ProModelExtraHigh,
        Fault::ProModelLegacyRow,
        Fault::LegacyRowPro,
        Fault::ChangedModelOwner,
        Fault::WrongTier,
        Fault::GenericUnavailable,
        Fault::CleanupFailed,
    ] {
        let f = fixture(fault, false).await;
        assert!(run(&f).await.is_err(), "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        let retired = !matches!(fault, Fault::CleanupFailed);
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(retired));
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "error");
        assert_eq!(meta["models"][0]["status"], "error");
        assert_eq!(meta["recoveryRequired"], !retired);
        assert_eq!(
            f.profile.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty(),
            retired
        );
        let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
        let record = TargetJournal::open(&f.store)
            .unwrap()
            .read(&creator["allocationId"])
            .unwrap();
        assert_eq!(record["disposition"]["decision"] == "retired", retired);
        stop(f).await;
    }
}
#[tokio::test]
async fn pre_allocation_bringup_failure_releases_only_its_unissued_admission() {
    for cancelled in [true, false] {
        let f = fixture(Fault::None, false).await;
        let foreign = f
            .profile
            .acquire(
                "PUBLIC-other-owner",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let foreign_before = f.profile.records().unwrap()["leases"][0].clone();
        let held = DirectoryLock::acquire(
            &f.profile.path().join("oracle-browser-bringup.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let observe = if cancelled {
            let profile = f.profile.clone();
            let id = f.id.clone();
            let token = f.cancel.clone();
            Some(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if profile.records().unwrap()["leases"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|row| row["sessionId"] == id)
                        {
                            token.cancel();
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
                .await
                .unwrap();
            }))
        } else {
            None
        };
        let mut selected = limits();
        // Cancellation is triggered by observed admission; only the deadline case uses a short budget.
        selected.admission = if cancelled {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(250)
        };
        selected.bringup = selected.admission;
        let result = execute(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &f.identity,
            &selected,
            &f.cancel,
        )
        .await;
        if let Some(observe) = observe {
            observe.await.unwrap();
        }
        held.release().unwrap();
        assert!(result.is_err());
        assert_eq!(
            f.profile.records().unwrap()["leases"],
            json!([foreign_before]),
            "cancelled={cancelled}: {result:?}"
        );
        assert!(
            f.trace
                .lock()
                .unwrap()
                .iter()
                .all(|request| request["method"] != "Target.createTarget")
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(
            meta["status"],
            if cancelled { "cancelled" } else { "error" }
        );
        assert_eq!(meta["recoveryRequired"], false);
        foreign.release().unwrap();
        stop(f).await;
    }
}
#[tokio::test]
async fn settled_pre_dispatch_cancellation_honours_keep_browser_and_explicit_close_policy() {
    for (fault, retired) in [
        (Fault::CancelPickerDefault, true),
        (Fault::CancelPickerKeep, false),
        (Fault::CancelPickerKeepClose, true),
        (Fault::CancelPickerRefuse, false),
    ] {
        let f = fixture(fault, false).await;
        let config = f.store.require(&f.id).unwrap()["browser"]["config"].clone();
        assert_eq!(
            config["keepBrowser"] == true,
            matches!(
                fault,
                Fault::CancelPickerKeep | Fault::CancelPickerKeepClose
            )
        );
        assert_eq!(
            config["closeOwnedTabOnCancel"].as_bool(),
            match fault {
                Fault::CancelPickerKeepClose => Some(true),
                Fault::CancelPickerRefuse => Some(false),
                _ => None,
            }
        );
        assert_eq!(
            run(&f).await.unwrap_err().to_string(),
            "Picker evaluation was cancelled or uncertain; preserving recovery"
        );
        let metadata = f.store.require(&f.id).unwrap();
        assert_eq!(metadata["status"], "cancelled");
        assert_eq!(metadata["models"][0]["status"], "cancelled");
        assert_eq!(metadata["recoveryRequired"], !retired);
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(retired));
        assert_eq!(
            f.profile.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty(),
            retired
        );
        stop(f).await;
    }
}
#[tokio::test]
async fn pre_dispatch_disposal_preserves_changed_marker_lease_and_unconfirmed_quarantine() {
    for fault in [
        Fault::PreDispatchForeignMarker,
        Fault::PreDispatchLeaseReplacement,
        Fault::PreDispatchQuarantineFailed,
    ] {
        let f = fixture(fault, false).await;
        let error = run(&f).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Picker selection is unverified (selected model does not match); refusing to submit."
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        let metadata = f.store.require(&f.id).unwrap();
        assert_eq!(metadata["recoveryRequired"], true);
        let rows = f.profile.records().unwrap();
        assert_eq!(rows["leases"].as_array().unwrap().len(), 1);
        if matches!(fault, Fault::PreDispatchLeaseReplacement) {
            assert_eq!(rows["leases"][0]["sessionId"], "PUBLIC replacement lease");
        }
        let creator = &metadata["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
        let record = TargetJournal::open(&f.store)
            .unwrap()
            .read(&creator["allocationId"])
            .unwrap();
        assert_ne!(record["disposition"]["decision"], "retired");
        if !matches!(fault, Fault::PreDispatchQuarantineFailed) {
            assert_eq!(
                record["disposition"]["reason"],
                "settled-pre-dispatch-close-pending"
            );
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn pre_dispatch_refused_close_reuses_its_durable_marker_without_sending() {
    let f = fixture(Fault::PreDispatchCloseRefused, false).await;
    let error = run(&f).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Picker selection is unverified (selected model does not match); refusing to submit."
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert_eq!(f.closes.load(Ordering::SeqCst), 2);
    let saved = f.store.require(&f.id).unwrap();
    let creator = &saved["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let journal = TargetJournal::open(&f.store).unwrap();
    let record = journal.read(&creator["allocationId"]).unwrap();
    assert!(record["preDispatchRetirement"]["id"].is_string());
    assert_eq!(record["disposition"]["decision"], "retired");
    assert_eq!(
        saved["browser"]["runtime"]["targetDispositionPending"],
        false
    );
    assert!(
        f.profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    stop(f).await;
}
#[tokio::test]
async fn source_mismatch_historical_echo_incomplete_answer_and_lost_ack_never_replay_or_retire() {
    for fault in [
        Fault::MissingSource,
        Fault::MissingWitness,
        Fault::ForeignWitness,
        Fault::ConflictingWitness,
        Fault::WrongSource,
        Fault::HistoricalEcho,
        Fault::Incomplete,
        Fault::LostSendAck,
        Fault::CancelSend,
    ] {
        let f = fixture(fault, false).await;
        assert!(run(&f).await.is_err(), "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        // Reference 15c8b9a: a cancelled run closes its own settled tab and frees its slot.
        let cancelled = matches!(fault, Fault::CancelSend);
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(cancelled));
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["recoveryRequired"], !cancelled);
        assert_eq!(meta["dispatch"]["state"], "uncertain");
        assert!(meta.get("capturedAnswer").is_none());
        if matches!(fault, Fault::MissingSource | Fault::WrongSource) {
            // Reference 46c757d: an unconfirmed receipt keeps the conversation it reached.
            let runtime = &meta["browser"]["runtime"];
            assert_eq!(runtime["tabUrl"], "https://chatgpt.com/c/consult-public");
            assert_eq!(runtime["conversationId"], "consult-public");
            assert!(runtime.get("userCommitReceipt").is_none());
        }
        assert!(run(&f).await.is_err());
        if matches!(
            fault,
            Fault::MissingWitness | Fault::ForeignWitness | Fault::ConflictingWitness
        ) {
            assert!(
                recover_existing(
                    &f.store,
                    &f.id,
                    f.profile.clone(),
                    f.cdp.clone(),
                    f.endpoint.clone(),
                    &limits(),
                    &f.cancel
                )
                .await
                .is_err()
            );
        }
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        stop(f).await;
    }
}
#[tokio::test]
async fn owned_upload_must_reach_readiness_and_cleanup_before_send() {
    for fault in [Fault::None, Fault::UploadFailed] {
        let f = fixture(fault, true).await;
        let result = run(&f).await;
        assert_eq!(result.is_ok(), matches!(fault, Fault::None));
        assert_eq!(
            f.sends.load(Ordering::SeqCst),
            usize::from(matches!(fault, Fault::None))
        );
        let trace = f.trace.lock().unwrap().clone();
        assert_eq!(
            trace
                .iter()
                .filter(|v| v["method"] == "DOM.setFileInputFiles")
                .count(),
            1,
            "{fault:?}: {result:?}"
        );
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        assert_eq!(f.store.require(&f.id).unwrap()["recoveryRequired"], false);
        let selected = trace
            .iter()
            .position(|v| {
                v["params"]["expression"]
                    .as_str()
                    .is_some_and(|s| s.contains("industrious-dom:thinking"))
            })
            .unwrap();
        let uploaded = trace
            .iter()
            .position(|v| v["method"] == "DOM.setFileInputFiles")
            .unwrap();
        assert!(selected < uploaded);
        assert!(f.home.path().join("public.txt").exists());
        stop(f).await;
    }
}
#[tokio::test]
async fn unconfirmed_upload_commands_or_cleanup_never_authorise_close_or_send() {
    for fault in [
        Fault::UploadObjectUnacknowledged,
        Fault::UploadDescribeUnacknowledged,
        Fault::UploadCommandUnacknowledged,
        Fault::UploadCleanupFailed,
        Fault::UploadCleanupWrongId,
        Fault::UploadCleanupWrongOwner,
        Fault::UploadCleanupWrongNavigation,
    ] {
        let f = fixture(fault, true).await;
        assert!(run(&f).await.is_err(), "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 0, "{fault:?}");
        assert_eq!(f.closes.load(Ordering::SeqCst), 0, "{fault:?}");
        assert_eq!(
            f.store.require(&f.id).unwrap()["recoveryRequired"],
            true,
            "{fault:?}"
        );
        assert_eq!(
            f.profile.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{fault:?}"
        );
        stop(f).await;
    }
}
#[tokio::test]
async fn a_cleaned_second_upload_failure_retires_the_owned_target_with_its_first_upload() {
    let f = fixture(Fault::SecondUploadFailed, true).await;
    let request = request(&f.store, &f.id).unwrap();
    assert_eq!(request.upload_paths.len(), 2);
    assert!(run(&f).await.is_err());
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    let meta = f.store.require(&f.id).unwrap();
    assert_eq!(
        meta["browser"]["runtime"]["uploads"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(meta["recoveryRequired"], false);
    assert!(!request.staged_uploads.unwrap().directory.exists());
    let trace = f.trace.lock().unwrap().clone();
    assert_eq!(
        trace
            .iter()
            .filter(|r| r["method"] == "DOM.setFileInputFiles")
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|r| r["params"]["expression"]
                .as_str()
                .is_some_and(|s| s.contains("industrious-dom:file-input-clean")))
            .count(),
        2
    );
    assert!(f.home.path().join("public.txt").exists());
    assert!(f.home.path().join("public-second.pdf").exists());
    stop(f).await;
}
#[tokio::test]
async fn stale_controller_cannot_capture_a_late_reply_and_foreign_retirement_preserves_capture() {
    let f = fixture(Fault::ReplacedOwner, false).await;
    assert!(run(&f).await.is_err());
    let meta = f.store.require(&f.id).unwrap();
    assert_eq!(meta["startedAt"], "replacement-controller");
    assert!(meta.get("capturedAnswer").is_none());
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    stop(f).await;
    let f = fixture(Fault::ForeignRetirement, false).await;
    let result = run(&f).await.unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["recoveryRequired"], true);
    assert!(result["cleanupWarning"].is_string());
    assert!(!captured(&f.store, &f.id).unwrap().is_empty());
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    stop(f).await;
}
#[tokio::test]
async fn explicit_lost_send_recovery_reads_original_commitment_without_a_second_send() {
    for mode in [
        "terminal",
        "orphan",
        "orphan-model-error",
        "attached-cancel",
    ] {
        let f = fixture(Fault::LostSendAck, false).await;
        assert!(run(&f).await.is_err());
        if mode != "terminal" {
            let mut previous = serde_json::to_value(ProcessIdentity::current().unwrap()).unwrap();
            previous["nativeStart"] = json!("1");
            previous["startedAtMs"] = json!(1.0);
            let dead: ProcessIdentity = serde_json::from_value(previous.clone()).unwrap();
            assert_eq!(dead.liveness(), Liveness::Dead);
            let model = f.store.require(&f.id).unwrap()["model"]
                .as_str()
                .unwrap()
                .to_owned();
            f.store.update_session_and_model(&f.id,&model,|meta,_|Ok(Some((json!({"status":"running","recoveryRequired":false,"lifecycle":merge_object(&meta["lifecycle"],&json!({"engine":"browser","processIdentity":previous,"workerPid":dead.pid,"workerProcessStartedAtMs":dead.started_at_ms}))}),Some(json!({"status":"running"})))))).unwrap();
        }
        let saved = f.store.require(&f.id).unwrap();
        let metadata_before =
            fs::read(f.store.directory(&f.id).unwrap().join("meta.json")).unwrap();
        assert_eq!(saved["dispatch"]["state"], "uncertain");
        let original_prompt = request(&f.store, &f.id).unwrap().prompt;
        let (client, server) = tokio::io::duplex(1024 * 1024);
        let recovery = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let socket = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let trace = Arc::new(Mutex::new(vec![]));
        let recovery_cancel = CancellationToken::new();
        let peer = tokio::spawn(
            Exchange {
                fault: if mode == "orphan-model-error" {
                    Fault::OrphanModelError
                } else if mode == "attached-cancel" {
                    Fault::CancelOutcome
                } else {
                    Fault::None
                },
                prompt: original_prompt,
                url: "https://chatgpt.com/c/consult-public".into(),
                owner: saved["browser"]["runtime"]["submissionOwner"].clone(),
                commit: Value::Null,
                before: false,
                turns: vec![],
                sends: f.sends.clone(),
                closes: f.closes.clone(),
                closed: false,
                trace: trace.clone(),
                cancel: recovery_cancel.clone(),
                store: f.store.clone(),
                id: f.id.clone(),
                download_stage: None,
                result_reads: 0,
                recovery_observed: false,
                download_count: 0,
                delayed_image_visible: false,
                retirement_marker: None,
                pending_retirement_read: false,
                cleanup_lock: None,
            }
            .run(socket),
        );
        let result = if mode == "attached-cancel" {
            recover_attached(
                &f.store,
                &f.id,
                f.profile.clone(),
                recovery.clone(),
                f.endpoint.clone(),
                &limits(),
                &recovery_cancel,
            )
            .await
        } else {
            recover_existing(
                &f.store,
                &f.id,
                f.profile.clone(),
                recovery.clone(),
                f.endpoint.clone(),
                &limits(),
                &recovery_cancel,
            )
            .await
        };
        if mode == "attached-cancel" {
            assert!(result.is_err());
            assert!(recovery_cancel.is_cancelled());
            let cancelled = f.store.require(&f.id).unwrap();
            assert_eq!(cancelled["status"], "error");
            assert_eq!(cancelled["recoveryRequired"], true);
            assert!(cancelled["browser"]["capturedAnswer"].is_null());
            assert_eq!(cancelled["dispatch"], saved["dispatch"]);
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
            assert_eq!(f.closes.load(Ordering::SeqCst), 0);
            let retried = recover_attached(
                &f.store,
                &f.id,
                f.profile.clone(),
                recovery.clone(),
                f.endpoint.clone(),
                &limits(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(retried["status"], "completed");
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
            assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        } else if mode == "orphan-model-error" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("no longer nonterminal")
            );
            assert_eq!(
                fs::read(f.store.directory(&f.id).unwrap().join("meta.json")).unwrap(),
                metadata_before
            );
            let current = f.store.require(&f.id).unwrap();
            assert_eq!(current["status"], "running");
            assert_eq!(current["models"][0]["status"], "error");
            assert_eq!(
                current["models"][0]["marker"],
                "PUBLIC concurrent terminal model"
            );
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
            assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        } else {
            let result = result.unwrap();
            assert_eq!(result["status"], "completed");
            assert_eq!(result["recoveryRequired"], false);
            assert_eq!(result["models"][0]["status"], "completed");
            assert_eq!(
                result["browser"]["runtime"]["userCommitReceipt"]["messageId"],
                "user-public"
            );
            assert_eq!(
                result["browser"]["thinkingSelection"],
                saved["browser"]["runtime"]["thinkingSelection"]
            );
            assert_eq!(
                result["browser"]["modelSelection"],
                saved["browser"]["runtime"]["modelSelection"]
            );
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
            assert_eq!(f.closes.load(Ordering::SeqCst), 1);
            assert!(trace.lock().unwrap().iter().all(|v| !matches!(
                v["method"].as_str(),
                Some("Target.createTarget" | "Input.dispatchKeyEvent" | "DOM.setFileInputFiles")
            )));
            assert!(!captured(&f.store, &f.id).unwrap().is_empty());
        }
        recovery
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
        stop(f).await;
    }
}
#[test]
fn running_recovery_requires_dead_native_worker_bound_scalars_and_nonterminal_model() {
    let current = ProcessIdentity::current().unwrap();
    let mut historical = serde_json::to_value(&current).unwrap();
    historical["nativeStart"] = json!("1");
    historical["startedAtMs"] = json!(1.0);
    let dead: ProcessIdentity = serde_json::from_value(historical.clone()).unwrap();
    assert_eq!(dead.liveness(), Liveness::Dead);
    let meta = json!({"mode":"browser","status":"running","recoveryRequired":false,"model":"gpt-6-pro","models":[{"model":"gpt-6-pro","status":"running"}],"lifecycle":{"engine":"browser","processIdentity":historical,"workerPid":dead.pid,"workerProcessStartedAtMs":dead.started_at_ms}});
    require_recovery_admission(&meta, &current).unwrap();
    for variant in [
        "current",
        "unknown",
        "pid",
        "start",
        "engine",
        "missing-model",
        "model-unknown",
        "completed",
        "partial",
        "cancelled",
        "error",
    ] {
        let mut changed = meta.clone();
        match variant {
            "current" => {
                changed["lifecycle"]["processIdentity"] = json!(current);
                changed["lifecycle"]["workerPid"] = json!(current.pid);
                changed["lifecycle"]["workerProcessStartedAtMs"] = json!(current.started_at_ms);
            }
            "unknown" => changed["lifecycle"]["processIdentity"]["nativeStart"] = json!("0"),
            "pid" => changed["lifecycle"]["workerPid"] = json!(0),
            "start" => changed["lifecycle"]["workerProcessStartedAtMs"] = json!(2.0),
            "engine" => changed["lifecycle"]["engine"] = json!("api"),
            "missing-model" => changed["models"] = json!([]),
            "model-unknown" => changed["models"][0]["status"] = json!("unrecognised"),
            status => changed["models"][0]["status"] = json!(status),
        }
        assert!(
            require_recovery_admission(&changed, &current).is_err(),
            "{variant}"
        );
    }
    let mut terminal = meta;
    terminal["status"] = json!("error");
    terminal["recoveryRequired"] = json!(true);
    terminal["lifecycle"]["processIdentity"] = json!(current);
    require_recovery_admission(&terminal, &current).unwrap();
    terminal["status"] = json!("cancelled");
    assert!(require_recovery_admission(&terminal, &current).is_err());
}
#[tokio::test]
async fn partial_image_recovery_continues_missing_publications_without_replaying_the_prompt() {
    let f = fixture(Fault::PartialImage, false).await;
    assert!(run(&f).await.is_err());
    let saved = f.store.require(&f.id).unwrap();
    let first = saved["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "image")
        .unwrap()
        .clone();
    let first_bytes = fs::read(first["path"].as_str().unwrap()).unwrap();
    let recovered = recover_existing(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &limits(),
        &f.cancel,
    )
    .await
    .unwrap();
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["recoveryRequired"], false);
    assert_eq!(
        recovered["browser"]["runtime"]["imageDelivery"]["savedImages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        fs::read(first["path"].as_str().unwrap()).unwrap(),
        first_bytes
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .filter(|operation| operation["params"]["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("industrious-dom:download-click")))
            .count(),
        3
    );
    stop(f).await;
}
#[tokio::test]
async fn download_guid_frame_image_validation_and_verified_publication_share_the_owned_lifecycle() {
    for fault in [
        Fault::File,
        Fault::Image,
        Fault::FormattedImage,
        Fault::ImageOnly,
        Fault::BadImage,
        Fault::ForeignDownloadFrame,
        Fault::DuplicateDownloadGuid,
        Fault::LostDownloadAck,
    ] {
        let f = fixture(fault, false).await;
        let result = run(&f).await;
        let success = matches!(
            fault,
            Fault::File | Fault::Image | Fault::FormattedImage | Fault::ImageOnly | Fault::BadImage
        );
        assert_eq!(result.is_ok(), success, "{fault:?}: {result:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(success));
        let meta = f.store.require(&f.id).unwrap();
        assert!(!captured(&f.store, &f.id).unwrap().is_empty());
        if matches!(fault, Fault::BadImage) {
            assert_eq!(meta["artifacts"][0]["code"], "image-download-failed");
            assert_eq!(meta["browser"]["runtime"]["imageDelivery"]["imageCount"], 1);
            assert!(
                meta["browser"]["runtime"]["imageDelivery"]["savedImages"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                captured(&f.store, &f.id).unwrap(),
                b"PUBLIC exact owned answer\n\nEnd\n"
            );
        } else if success {
            let artifact = &meta["artifacts"][0];
            let bytes = fs::read(artifact["path"].as_str().unwrap()).unwrap();
            assert_eq!(artifact["sha256"], sha256(&bytes));
            assert_eq!(
                artifact["origin"]["downloadGuid"],
                "33333333-3333-4333-8333-333333333333"
            );
            assert_eq!(
                artifact["kind"],
                if matches!(
                    fault,
                    Fault::Image | Fault::FormattedImage | Fault::ImageOnly
                ) {
                    "image"
                } else {
                    "file"
                }
            );
            if matches!(fault, Fault::FormattedImage) {
                assert!(
                    String::from_utf8(captured(&f.store, &f.id).unwrap())
                        .unwrap()
                        .starts_with("**PUBLIC exact owned answer**")
                );
            }
            if matches!(fault, Fault::ImageOnly) {
                let text = String::from_utf8(captured(&f.store, &f.id).unwrap()).unwrap();
                assert!(
                    text.starts_with("Generated image.\n\n*Generated 1 image(s). Saved to: ")
                        && text.ends_with('*')
                );
            }
        } else {
            assert_eq!(meta["recoveryRequired"], true);
            assert!(run(&f).await.is_err());
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn recovery_preserves_image_positions_and_reuses_button_publications_after_output_failure() {
    for fault in [Fault::FirstBadMultiImage, Fault::ButtonOutputFailure] {
        let f = fixture(fault, false).await;
        assert!(run(&f).await.is_err());
        let before = f.store.require(&f.id).unwrap();
        let images = before["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|artifact| artifact["kind"] == "image")
            .count();
        assert_eq!(
            images,
            usize::from(matches!(fault, Fault::ButtonOutputFailure))
        );
        if matches!(fault, Fault::ButtonOutputFailure) {
            assert_eq!(
                fs::read(f.home.path().join("PUBLIC-output.md/PUBLIC-sentinel.txt")).unwrap(),
                b"PUBLIC preserved output"
            );
            fs::rename(
                f.home.path().join("PUBLIC-output.md"),
                f.home.path().join("PUBLIC-preserved-output-directory"),
            )
            .unwrap();
        } else {
            assert!(
                !f.home.path().join("PUBLIC-image.png").exists()
                    && !f.home.path().join("PUBLIC-image.2.png").exists()
            );
        }
        let recovered = recover_existing(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &limits(),
            &f.cancel,
        )
        .await
        .unwrap();
        assert_eq!(recovered["status"], "completed");
        assert_eq!(recovered["recoveryRequired"], false);
        assert_eq!(
            recovered["browser"]["runtime"]["imageDelivery"]["imageCount"],
            if matches!(fault, Fault::ButtonOutputFailure) {
                1
            } else {
                2
            }
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.trace
                .lock()
                .unwrap()
                .iter()
                .filter(|operation| {
                    operation["params"]["expression"]
                        .as_str()
                        .is_some_and(|expression| {
                            expression.contains("industrious-dom:download-click")
                        })
                })
                .count(),
            if matches!(fault, Fault::ButtonOutputFailure) {
                1
            } else {
                3
            }
        );
        if matches!(fault, Fault::ButtonOutputFailure) {
            assert_eq!(
                fs::read(f.home.path().join("PUBLIC-output.md")).unwrap(),
                crate::browser_output::required_answer_bytes(
                    &recovered["browser"]["requiredTextOutput"],
                    &recovered,
                    &captured(&f.store, &f.id).unwrap()
                )
                .unwrap()
            );
        }
        stop(f).await;
    }
}
#[test]
fn invalid_image_destinations_refuse_before_session_creation() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    for key in ["generateImage", "outputPath"] {
        let mut input = json!({"prompt":"PUBLIC image consultation requested","engine":"browser"});
        input[key] = json!(true);
        let resolved = crate::execution::resolve(
            &input,
            &json!({}),
            &crate::config::Environment::new(),
            home.path(),
        )
        .unwrap();
        assert!(
            prepare(&store, &resolved, home.path())
                .unwrap_err()
                .to_string()
                .contains("path string")
        );
        assert!(!store.home().exists());
    }
}
#[tokio::test]
async fn persisted_button_images_never_bypass_an_uncertain_second_download() {
    let f = fixture(Fault::MultiButtonLostImage, false).await;
    assert!(run(&f).await.is_err());
    let before = f.store.require(&f.id).unwrap();
    assert_eq!(
        before["browser"]["runtime"]["downloadContextSettled"],
        false
    );
    assert!(
        Path::new(
            before["browser"]["runtime"]["downloadStage"]
                .as_str()
                .unwrap()
        )
        .is_dir()
    );
    assert_eq!(
        before["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|artifact| artifact["kind"] == "image")
            .count(),
        1
    );
    let outcome = recover_existing(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &limits(),
        &f.cancel,
    )
    .await;
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("download ownership is unsettled")
    );
    f.store
        .update(&f.id, |metadata| {
            let mut browser = metadata["browser"].clone();
            browser["runtime"]["downloadStage"] = Value::Null;
            Ok(Some(json!({"browser":browser})))
        })
        .unwrap();
    let absent_stage = recover_existing(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &limits(),
        &f.cancel,
    )
    .await;
    assert!(
        absent_stage
            .unwrap_err()
            .to_string()
            .contains("download ownership is unsettled")
    );
    let after = f.store.require(&f.id).unwrap();
    assert_eq!(after["recoveryRequired"], true);
    assert_eq!(after["browser"]["runtime"]["downloadContextSettled"], false);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .filter(|operation| operation["params"]["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("industrious-dom:download-click")))
            .count(),
        2
    );
    stop(f).await;
}
#[tokio::test(start_paused = true)]
async fn required_image_paths_counts_late_inventory_and_failures_keep_one_dispatch() {
    for fault in [
        Fault::NamedImage,
        Fault::OutputImage,
        Fault::MultiImage,
        Fault::DuplicateImage,
        Fault::ExistingImage,
        Fault::MissingImage,
        Fault::DelayedImage,
        Fault::ButtonImage,
        Fault::LateButtonImage,
        Fault::MultiButtonImage,
        Fault::NonImageButton,
        Fault::NonImageThenImage,
        Fault::InvalidDirectButtonImage,
        Fault::DelayedBadImage,
    ] {
        let f = fixture(fault, false).await;
        let outcome = run(&f).await;
        let success = !matches!(
            fault,
            Fault::ExistingImage
                | Fault::MissingImage
                | Fault::NonImageButton
                | Fault::DelayedBadImage
        );
        assert_eq!(outcome.is_ok(), success, "{fault:?}: {outcome:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.closes.load(Ordering::SeqCst), usize::from(success));
        let metadata = f.store.require(&f.id).unwrap();
        assert!(!captured(&f.store, &f.id).unwrap().is_empty());
        if success {
            let expected = if matches!(fault, Fault::MultiImage | Fault::MultiButtonImage) {
                2
            } else {
                1
            };
            let summary = &metadata["browser"]["runtime"]["imageDelivery"];
            if matches!(fault, Fault::DuplicateImage) {
                assert_eq!(summary["generatedImages"][0]["width"], 24);
                assert_eq!(summary["savedImages"][0]["alt"], "Generated larger image");
                assert!(!f.home.path().join("PUBLIC-image.2.png").exists());
            }
            assert_eq!(summary["imageCount"], expected);
            assert_eq!(summary["savedImages"].as_array().unwrap().len(), expected);
            for image in summary["savedImages"].as_array().unwrap() {
                let path = image["path"].as_str().unwrap();
                let bytes = fs::read(path).unwrap();
                assert_eq!(image["sha256"], sha256(&bytes));
                assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            }
            assert!(f.home.path().join("PUBLIC-image.png").exists());
            if expected == 2 {
                assert!(f.home.path().join("PUBLIC-image.2.png").exists());
            }
            let answer = String::from_utf8(captured(&f.store, &f.id).unwrap()).unwrap();
            assert!(answer.contains(&format!("*Generated {expected} image(s). Saved")));
        } else {
            assert_eq!(metadata["recoveryRequired"], true);
            if matches!(fault, Fault::ExistingImage) {
                assert_eq!(
                    fs::read(f.home.path().join("PUBLIC-image.png")).unwrap(),
                    b"PUBLIC existing image destination"
                );
            }
            if matches!(fault, Fault::MissingImage | Fault::NonImageButton) {
                assert!(
                    metadata["errorMessage"]
                        .as_str()
                        .unwrap()
                        .starts_with("No images generated.")
                );
                assert!(!f.home.path().join("PUBLIC-image.png").exists());
            }
        }
        stop(f).await;
    }
}
// Live receipt L5a: a home with forward slashes reached Chrome as the download directory, so the
// owned image button download was reported canceled and the run quarantined its context.
#[cfg(windows)]
#[tokio::test(start_paused = true)]
async fn image_button_download_survives_a_home_with_forward_slashes() {
    let mut f = fixture(Fault::ButtonImage, false).await;
    f.store = SessionStore::new(f.store.home().to_str().unwrap().replace('\\', "/"));
    let outcome = run(&f).await;
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    let behaviour = f
        .trace
        .lock()
        .unwrap()
        .iter()
        .find(|request| request["params"]["behavior"] == "allowAndName")
        .unwrap()["params"]["downloadPath"]
        .clone();
    assert!(!behaviour.as_str().unwrap().contains('/'), "{behaviour}");
    let metadata = f.store.require(&f.id).unwrap();
    assert_eq!(
        metadata["browser"]["runtime"]["downloadContextSettled"],
        true
    );
    let saved = &metadata["browser"]["runtime"]["imageDelivery"]["savedImages"][0];
    assert!(
        fs::read(saved["path"].as_str().unwrap())
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    stop(f).await;
}

fn methods(f: &Fixture) -> Vec<String> {
    f.trace
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap_or("").to_owned())
        .collect()
}
fn submission_actions(f: &Fixture) -> Vec<String> {
    f.trace
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request["params"]["expression"]
                .as_str()
                .is_some_and(|e| e.contains("industrious-dom:submission"))
        })
        .filter_map(|request| {
            let expression = request["params"]["expression"].as_str()?;
            [
                "acquire",
                "assert",
                "releasing",
                "clean",
                "quarantine",
                "dispatch",
                "acknowledge",
                "release",
            ]
            .into_iter()
            .find(|action| expression.contains(&format!("\"action\":\"{action}\"")))
            .map(str::to_owned)
        })
        .collect()
}
#[tokio::test]
async fn borrowed_tab_submits_once_with_verified_selection_and_is_never_created_or_closed() {
    let f = fixture(Fault::Borrowed, false).await;
    let meta = run(&f).await.unwrap();
    assert_eq!(meta["status"], "completed");
    assert_eq!(meta["recoveryRequired"], false);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    let methods = methods(&f);
    assert!(
        !methods
            .iter()
            .any(|m| m == "Target.createTarget" || m == "Target.closeTarget")
    );
    assert!(!methods.iter().any(|m| m == "Page.navigate"));
    let runtime = &meta["browser"]["runtime"];
    assert!(
        runtime
            .get("ownedRecoveryTarget")
            .is_none_or(Value::is_null)
    );
    assert_eq!(runtime["borrowedTarget"]["targetId"], "TARGET-PUBLIC");
    assert_eq!(
        runtime["borrowedTarget"]["conversationId"],
        "consult-public"
    );
    assert_eq!(runtime["chromeTargetId"], "TARGET-PUBLIC");
    // Node verifyThinkingTime: verification alone saves an already-selected status.
    assert_eq!(runtime["thinkingSelection"]["status"], "already-selected");
    assert!(
        runtime["thinkingSelection"]
            .get("targetModelKind")
            .is_none()
    );
    let actions = submission_actions(&f);
    assert_eq!(actions.first().map(String::as_str), Some("acquire"));
    assert_eq!(
        actions.last().map(String::as_str),
        Some("release"),
        "{actions:?}"
    );
    stop(f).await;
}
#[tokio::test]
async fn borrowed_tab_in_work_mode_opens_the_configured_chat_before_claiming_it() {
    let f = fixture(Fault::BorrowedWork, false).await;
    let meta = run(&f).await.unwrap();
    assert_eq!(meta["status"], "completed");
    let trace = f.trace.lock().unwrap().clone();
    let navigate = trace
        .iter()
        .position(|r| r["method"] == "Page.navigate")
        .expect("Work mode must be left by navigation");
    assert_eq!(
        trace[navigate]["params"]["url"],
        "https://chatgpt.com/?temporary-chat=true"
    );
    let acquire = trace
        .iter()
        .position(|r| {
            r["params"]["expression"].as_str().is_some_and(|e| {
                e.contains("industrious-dom:submission") && e.contains("\"action\":\"acquire\"")
            })
        })
        .unwrap();
    assert!(navigate < acquire);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    stop(f).await;
}
#[tokio::test]
async fn borrowed_tab_refusals_never_send_or_close_and_retain_acquired_claims_for_cleanup() {
    for (fault, message) in [
        (Fault::BorrowedWrongTier, "thinking"),
        (Fault::BorrowedClaimed, "active or uncertain ownership"),
    ] {
        let f = fixture(fault, false).await;
        let error = run(&f).await.unwrap_err();
        assert!(
            format!("{error:#}").to_lowercase().contains(message),
            "{error:#}"
        );
        if matches!(fault, Fault::BorrowedClaimed) {
            assert!(
                format!("{error:#}").contains(
                    "This Industrious target is busy or quarantined; choose another browser tab."
                ),
                "{error:#}"
            );
        }
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "error");
        assert_eq!(
            meta["recoveryRequired"],
            matches!(fault, Fault::BorrowedWrongTier)
        );
        assert!(
            meta["browser"]["runtime"]
                .get("ownedRecoveryTarget")
                .is_none_or(Value::is_null)
        );
        let actions = submission_actions(&f);
        if matches!(fault, Fault::BorrowedWrongTier) {
            assert_eq!(
                actions.last().map(String::as_str),
                Some("quarantine"),
                "{actions:?}"
            );
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn borrowed_tab_sessions_refuse_native_recovery_before_any_browser_operation() {
    let f = fixture(Fault::Borrowed, false).await;
    run(&f).await.unwrap();
    // As if the lease release had failed: the record asks for recovery it must never get.
    f.store
        .update(&f.id, |_| Ok(Some(json!({"recoveryRequired":true}))))
        .unwrap();
    let before = f.trace.lock().unwrap().len();
    let error = recover_existing(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &limits(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("holds no recovery authority"),
        "{error:#}"
    );
    assert_eq!(f.trace.lock().unwrap().len(), before);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    stop(f).await;
}

#[tokio::test]
async fn borrowed_payload_failures_keep_quarantine_and_upload_preflight_is_cleaned() {
    for fault in [
        Fault::BorrowedPartialPrompt,
        Fault::BorrowedSecondUploadPrepareFailed,
        Fault::BorrowedUploadAckFailed,
    ] {
        let f = fixture(fault, !matches!(fault, Fault::BorrowedPartialPrompt)).await;
        if matches!(fault, Fault::BorrowedSecondUploadPrepareFailed) {
            assert_eq!(
                request(&f.store, &f.id).unwrap().upload_paths.len(),
                2,
                "Fixture must retain 2 uploads"
            );
        }
        let error = match run(&f).await {
            Err(error) => error,
            Ok(_) => panic!("Borrowed payload failure unexpectedly completed: {fault:?}"),
        };
        assert!(
            format!("{error:#}").contains("borrowed tab cleanup required"),
            "{error:#}"
        );
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        assert_eq!(f.closes.load(Ordering::SeqCst), 0);
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["recoveryRequired"], true);
        assert_eq!(
            meta["browser"]["runtime"]["borrowedCleanup"]["state"],
            "manual-required"
        );
        let actions = submission_actions(&f);
        assert_eq!(
            actions.last().map(String::as_str),
            Some("quarantine"),
            "{actions:?}"
        );
        assert!(!actions.iter().any(|a| a == "release"));
        if !matches!(fault, Fault::BorrowedPartialPrompt) {
            let trace = f.trace.lock().unwrap();
            let cleanups = trace
                .iter()
                .filter(|r| {
                    r["params"]["expression"]
                        .as_str()
                        .is_some_and(|e| e.contains("industrious-dom:file-input-clean"))
                })
                .count();
            let transfers = trace
                .iter()
                .filter(|r| r["method"] == "DOM.setFileInputFiles")
                .count();
            assert_eq!(
                cleanups,
                if matches!(fault, Fault::BorrowedSecondUploadPrepareFailed) {
                    2
                } else {
                    1
                }
            );
            assert_eq!(
                transfers,
                usize::from(matches!(fault, Fault::BorrowedSecondUploadPrepareFailed))
            );
        }
        stop(f).await;
    }
}
#[tokio::test]
async fn borrowed_release_failure_keeps_completed_answer_and_explicit_manual_cleanup() {
    let f = fixture(Fault::BorrowedReleaseFailed, false).await;
    let meta = run(&f).await.unwrap();
    assert_eq!(meta["status"], "completed");
    assert!(meta["capturedAnswer"].is_object());
    assert_eq!(meta["recoveryRequired"], true);
    assert_eq!(
        meta["browser"]["runtime"]["borrowedCleanup"]["state"],
        "manual-required"
    );
    assert!(meta["browser"]["runtime"]["cleanupWarning"].is_string());
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert_eq!(
        submission_actions(&f).last().map(String::as_str),
        Some("quarantine")
    );
    stop(f).await;
}
/// A concurrent retry that proved exit keeps its receipt: a losing attempt's refusal,
/// recorded after it, must not replace `exited` with `retained`.
#[tokio::test]
async fn losing_idle_retry_keeps_the_exited_receipt() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(".oracle");
    let store = SessionStore::new(path.clone());
    let id = store
        .create(json!({"prompt":"PUBLIC idle launcher"}), home.path(), None)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Malformed saved authority refuses before any browser IO.
    store.update(&id,|current|Ok(Some(json!({"status":"completed","recoveryRequired":false,
        "browser":merge_object(&current["browser"],&json!({"runtime":{"persistentBrowserOwner":{"version":1,"kind":"PUBLIC-malformed"},
            "targetDispositionPending":false}}))})))).unwrap();
    let disposition = |store: &SessionStore| {
        store.stored_for_update(&id).unwrap()["browser"]["runtime"]["persistentBrowserDisposal"]
            ["disposition"]
            .clone()
    };
    // Alone, the refusal records retention.
    assert!(settle_saved_idle_now(&store, &id).await.is_none());
    assert_eq!(disposition(&store), "retained");
    let exited =
        json!({"version":1,"disposition":"exited","profileRetained":true,"naturalExit":true});
    let (concurrent, session, receipt) = (SessionStore::new(path), id.clone(), exited.clone());
    TEST_BEFORE_IDLE_RECEIPT.set(Some(Box::new(move || {
        concurrent.update(&session,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],
            &json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"persistentBrowserDisposal":receipt}))}))})))).unwrap();
    })));
    assert!(settle_saved_idle_now(&store, &id).await.is_none());
    assert!(TEST_BEFORE_IDLE_RECEIPT.take().is_none());
    assert_eq!(
        store.stored_for_update(&id).unwrap()["browser"]["runtime"]["persistentBrowserDisposal"],
        exited
    );
}

#[test]
fn terminal_gate_tunables_follow_the_reference_environment() {
    let env = |pairs: &[(&str, &str)]| -> Environment {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    assert_eq!(gate_tunables(&env(&[])), (3, 1_200));
    assert_eq!(
        gate_tunables(&env(&[
            ("ORACLE_BAR_CONFIRM_CYCLES", " 5.9 "),
            ("ORACLE_TERMINAL_MIN_STABLE_MS", "2500")
        ])),
        (5, 2_500)
    );
    for bad in ["0", "-2", "", "x", "Infinity", "NaN"] {
        assert_eq!(
            gate_tunables(&env(&[
                ("ORACLE_BAR_CONFIRM_CYCLES", bad),
                ("ORACLE_TERMINAL_MIN_STABLE_MS", bad)
            ])),
            (3, 1_200),
            "{bad:?}"
        );
    }
    // Reference BROWSER_BRINGUP_LOCK_TIMEOUT_MS is fixed at 10 minutes, unlike profile locks.
    assert_eq!(RunnerLimits::default().bringup, Duration::from_secs(600));
    assert_eq!(
        RunnerLimits::default().profile_lock,
        Duration::from_secs(300)
    );
}

#[test]
fn commit_and_attachment_budgets_follow_the_reference_formulas() {
    assert_eq!(
        RunnerLimits::from_config(&json!({"inputTimeoutMs":120_000})).commit,
        Duration::from_secs(120)
    );
    let base = RunnerLimits::from_config(&json!({}));
    assert_eq!(base.commit, Duration::from_secs(60));
    assert_eq!(
        attachment_budget(&json!({}), &base, 1),
        Duration::from_secs(60)
    );
    assert_eq!(
        attachment_budget(&json!({}), &base, 4),
        Duration::from_secs(120)
    );
    let short = RunnerLimits::from_config(&json!({"inputTimeoutMs":10_000}));
    assert_eq!(
        attachment_budget(&json!({}), &short, 1),
        Duration::from_secs(45)
    );
    assert_eq!(
        attachment_budget(&json!({"attachmentTimeoutMs":600_000}), &short, 4),
        Duration::from_secs(600)
    );
}

#[tokio::test]
async fn browser_runs_log_the_reference_control_plan() {
    let f = fixture(Fault::None, false).await;
    run(&f).await.unwrap();
    let log = f.store.log(&f.id).unwrap();
    assert!(
        log.contains(
            "[browser] Browser control: launch visible Chrome; may focus/control the browser UI.\n[browser] Browser guidance: "
        ),
        "{log}"
    );
    stop(f).await;
}

#[tokio::test]
async fn picker_failures_use_the_reference_errors_and_log_lines() {
    for (fault, error, line) in [
        (
            Fault::GenericUnavailable,
            "Thinking time: option not found (requested Pro); refusing to submit without confirmed Pro.",
            None,
        ),
        (
            Fault::DisabledProForeign,
            "Thinking time: Pro is unavailable on this account (PUBLIC weekly limit); refusing to submit without confirmed Pro.",
            None,
        ),
        (
            Fault::DisabledExtraHigh,
            "Browser model and thinking effort could not be verified; refusing to submit.",
            Some(
                "[browser] Confirmed owned Pro unavailability; trying GPT-6 with Extra High.\n[browser] Thinking time: Extra High is unavailable on this account (PUBLIC disabled row); keeping the effort already selected in ChatGPT.",
            ),
        ),
        (
            Fault::ModelThrottled,
            "ChatGPT is rate limiting this session: Too many requests. PUBLIC notice This is a temporary limit on the account, not a problem with the request — wait a few minutes and retry, and reduce how many conversations are started at once.",
            Some(
                "[browser] ChatGPT rate limit detected during model-selection: Too many requests. PUBLIC notice",
            ),
        ),
        (
            Fault::ModelsNotLoaded,
            "ChatGPT could not load its model list (Could not load ChatGPT models). The page is broken, not the request: reload the tab and retry.",
            Some(
                "[browser] ChatGPT model list failed to load during model-selection (model-button): Could not load ChatGPT models",
            ),
        ),
    ] {
        let f = fixture(fault, false).await;
        assert_eq!(run(&f).await.unwrap_err().to_string(), error, "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        if let Some(line) = line {
            let log = f.store.log(&f.id).unwrap();
            assert!(log.contains(line), "{log}");
        }
        stop(f).await;
    }
}

#[tokio::test]
async fn fresh_current_strategy_refuses_before_any_picker_inspection() {
    let f = fixture_attempt(
        Fault::None,
        false,
        false,
        json!({"browserModelStrategy":"current"}),
    )
    .await;
    assert_eq!(
        run(&f).await.unwrap_err().to_string(),
        "Picker selection is unverified (Requested browser model could not be verified; refusing to select thinking effort.); refusing to submit."
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert!(!f.trace.lock().unwrap().iter().any(|r| {
        r["params"]["expression"].as_str().is_some_and(|s| {
            s.contains("industrious-dom:thinking") || s.contains("industrious-dom:model")
        })
    }));
    stop(f).await;
}

#[tokio::test]
async fn picker_progress_reaches_the_session_log_and_saves_the_requested_alias() {
    let f = fixture_attempt(
        Fault::ModelPending,
        false,
        false,
        json!({"browserModelLabel":"Latest"}),
    )
    .await;
    // The progress line names the command budget.
    run_within(&f, &one_second_command_limits()).await.unwrap();
    let log = f.store.log(&f.id).unwrap();
    for line in [
        "Model picker button not mounted yet; waiting up to 1s for the composer pill to render.",
        "Model picker: GPT-6",
        "[browser] Thinking time: Pro",
    ] {
        assert!(log.contains(line), "{log}");
    }
    assert_eq!(
        f.store.require(&f.id).unwrap()["browser"]["runtime"]["modelSelection"]["requestedModel"],
        "Latest"
    );
    stop(f).await;
}

#[tokio::test]
async fn send_readiness_waits_for_the_exact_control_within_the_reference_budget() {
    let f = fixture(Fault::SendSettling, false).await;
    run(&f).await.unwrap();
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    stop(f).await;
    let f = fixture(Fault::SendAttachmentsNeverReady, true).await;
    assert_eq!(
        run(&f).await.unwrap_err().to_string(),
        "Attachments never reached the exact ready send button after 1s; tune --browser-attachment-timeout."
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    stop(f).await;
    let f = fixture(Fault::SendNeverReady, false).await;
    let timeout = Duration::from_secs(1);
    f.cdp
        .call(
            "Page.navigate",
            json!({"url":"https://chatgpt.com/"}),
            timeout,
            &f.cancel,
        )
        .await
        .unwrap();
    let mut reservation =
        RendererReservation::acquire(f.cdp.clone(), "claim-public".into(), timeout, &f.cancel)
            .await
            .unwrap();
    let (_, nav) = browser_dom::capture_navigation(&reservation, timeout, &f.cancel)
        .await
        .unwrap();
    let prepared = PreparedPrompt::stage(
        &mut reservation,
        "PUBLIC send readiness contract",
        &nav,
        &[],
        timeout,
        &f.cancel,
    )
    .await
    .unwrap();
    assert_eq!(
        prepared.send_budget(&json!({"attachmentTimeoutMs":600_000})),
        Duration::from_secs(20)
    );
    assert_eq!(
        prepared
            .arm_send(&mut reservation, Duration::from_millis(300), &f.cancel)
            .await
            .unwrap_err()
            .to_string(),
        "Send button did not settle before input timeout."
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    prepared.cleanup(&mut reservation, timeout).await.unwrap();
    stop(f).await;
}

#[tokio::test]
async fn conversation_list_rate_limit_logs_the_reference_warning_and_still_sends() {
    let f = fixture(Fault::ChatListLimited, false).await;
    run(&f).await.unwrap();
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    let log = f.store.log(&f.id).unwrap();
    assert!(
        log.contains(
            "[browser] ChatGPT is rate-limiting its conversation list (HTTP 429); attempting the composer send anyway."
        ),
        "{log}"
    );
    stop(f).await;
}

#[tokio::test]
async fn verbose_runs_log_the_reference_automation_lines_and_quiet_runs_omit_them() {
    for verbose in [true, false] {
        let f = fixture_attempt(Fault::None, true, false, json!({"verbose":verbose})).await;
        assert_eq!(run(&f).await.unwrap()["status"], "completed");
        let log = f.store.log(&f.id).unwrap();
        let conversation = f.store.require(&f.id).unwrap()["browser"]["runtime"]["conversationId"]
            .as_str()
            .unwrap()
            .to_owned();
        // Reference verbose-only automation lines, in run order.
        let lines = [
            "[browser] url = https://chatgpt.com/?temporary-chat=true".to_owned(),
            "[reattach] chrome port=9222 host=127.0.0.1 url=https://chatgpt.com/?temporary-chat=true target=TARGET-PUBLIC".into(),
            "Uploading attachment: public.txt".into(),
            "All attachments uploaded".into(),
            format!("[browser] conversation url (post-submit) = https://chatgpt.com/c/{conversation}"),
        ];
        let positions = lines
            .iter()
            .map(|line| log.find(&format!("{line}\n")))
            .collect::<Vec<_>>();
        if verbose {
            assert!(positions.iter().all(Option::is_some), "{log}");
            assert!(positions.is_sorted(), "{log}");
        } else {
            assert!(positions.iter().all(Option::is_none), "{log}");
        }
        // Always-printed reference lines stay in quiet runs.
        assert!(log.contains("[browser] ChatGPT archive skipped (temporary-chat).\n"));
        stop(f).await;
    }
}
#[tokio::test]
async fn quiet_runs_keep_the_reference_follow_up_and_image_wait_lines() {
    for (fault, lines) in [
        (
            Fault::Sequential,
            vec![
                "[browser] Sending follow-up 1/2",
                "[browser] Sending follow-up 2/2",
            ],
        ),
        (
            Fault::NamedImage,
            vec!["[browser] Waiting for ChatGPT generated image response."],
        ),
    ] {
        let f = fixture(fault, false).await;
        assert_eq!(run(&f).await.unwrap()["status"], "completed", "{fault:?}");
        let log = f.store.log(&f.id).unwrap();
        for line in lines {
            assert!(
                log.lines().any(|logged| logged == line),
                "{fault:?} {line}\n{log}"
            );
        }
        assert!(!log.contains("[reattach]"), "{log}");
        stop(f).await;
    }
}

#[test]
fn admission_waits_the_run_timeout_whilst_profile_locks_keep_their_own_budget() {
    let limits =
        RunnerLimits::from_config(&json!({"timeoutMs":1_200_000,"profileLockTimeoutMs":300_000}));
    assert_eq!(limits.admission, Duration::from_secs(1_200));
    assert_eq!(limits.profile_lock, Duration::from_secs(300));
    let defaults = RunnerLimits::from_config(&json!({}));
    assert_eq!(defaults.admission, Duration::from_secs(1_200));
    assert_eq!(defaults.profile_lock, Duration::from_secs(300));
    assert_eq!(
        RunnerLimits::from_config(&json!({"researchMode":"deep"})).admission,
        Duration::from_secs(2_400)
    );
}
fn journal_disposition(f: &Fixture) -> Value {
    let meta = f.store.require(&f.id).unwrap();
    let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    TargetJournal::open(&f.store)
        .unwrap()
        .read(&creator["allocationId"])
        .unwrap()["disposition"]
        .clone()
}
#[tokio::test]
async fn cancelled_send_settles_then_closes_its_own_tab_and_frees_the_slot_without_replay() {
    TEST_CANCELLED_SEND_SETTLE.set(Some(Duration::from_millis(300)));
    for (fault, reason) in [
        (Fault::CancelSend, "cancelled-send-unconfirmed-settled"),
        (Fault::CancelOutcome, "cancelled-after-send-settled"),
        (
            Fault::CancelSendGenerating,
            "cancelled-send-unconfirmed-timed-out",
        ),
    ] {
        let f = fixture(fault, false).await;
        let started = Instant::now();
        assert!(run(&f).await.is_err(), "{fault:?}");
        let meta = f.store.require(&f.id).unwrap();
        assert_eq!(meta["status"], "cancelled", "{fault:?}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1, "{fault:?}");
        assert_eq!(f.closes.load(Ordering::SeqCst), 1, "{fault:?}");
        if matches!(fault, Fault::CancelSendGenerating) {
            assert!(started.elapsed() >= Duration::from_millis(300));
        }
        assert_eq!(
            meta["browser"]["runtime"]["cancelledTargetDisposition"],
            reason
        );
        assert_eq!(
            meta["browser"]["runtime"]["targetDispositionPending"],
            false
        );
        assert_eq!(meta["recoveryRequired"], false, "{fault:?}");
        let disposition = journal_disposition(&f);
        assert_eq!(disposition["decision"], "retired");
        assert_eq!(disposition["absence"], "confirmed-absent");
        assert_eq!(disposition["reason"], reason);
        assert!(
            f.profile.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{fault:?}: the cancelled run must free its slot"
        );
        // The saved conversation stays recoverable; nothing is resubmitted.
        assert!(run(&f).await.is_err());
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        stop(f).await;
    }
    TEST_CANCELLED_SEND_SETTLE.set(None);
}
#[tokio::test]
async fn cancelled_send_with_a_refused_close_retains_its_tab_slot_and_recovery() {
    TEST_CANCELLED_SEND_SETTLE.set(Some(Duration::from_millis(100)));
    let f = fixture(Fault::CancelSendCloseRefused, false).await;
    assert!(run(&f).await.is_err());
    let meta = f.store.require(&f.id).unwrap();
    assert_eq!(meta["status"], "cancelled");
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    assert_eq!(meta["recoveryRequired"], true);
    assert!(
        meta["browser"]["runtime"]["cancelledSendCleanupWarning"]
            .as_str()
            .is_some_and(|warning| warning.contains("unconfirmed"))
    );
    let disposition = journal_disposition(&f);
    assert_eq!(disposition["decision"], "retained");
    assert_eq!(disposition["reason"], "cancelled-send-unconfirmed-settled");
    assert_eq!(
        f.profile.records().unwrap()["leases"][0]["targetDispositionPending"],
        true
    );
    TEST_CANCELLED_SEND_SETTLE.set(None);
    stop(f).await;
}
#[tokio::test]
async fn post_send_terminal_error_returns_and_its_slot_is_reclaimed_after_worker_exit() {
    let f = fixture(Fault::Incomplete, false).await;
    // The worker's process path returns its terminal error; it never waits on the retained tab.
    let error = tokio::time::timeout(Duration::from_secs(10), run(&f))
        .await
        .expect("post-send terminal error must return")
        .unwrap_err();
    assert!(!f.cancel.is_cancelled(), "{error:#}");
    let meta = f.store.require(&f.id).unwrap();
    assert_eq!(meta["status"], "error");
    assert_eq!(meta["recoveryRequired"], true);
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    let retained = f.profile.records().unwrap()["leases"][0].clone();
    assert_eq!(retained["targetDispositionPending"], true);
    // The worker exits: its record now names an exited controller and a stale heartbeat.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let exited = ProcessIdentity {
        pid,
        started_at_ms: 1_000_000_000_000.0,
        native_start: 7_654_321,
    };
    let registry_path = f.profile.path().join("oracle-tab-leases.json");
    let mut registry: Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    let lease = &mut registry["leases"][0];
    lease["pid"] = json!(pid);
    lease["processStartedAtMs"] = json!(exited.started_at_ms);
    lease["processIdentity"] = json!(exited);
    lease["updatedAt"] = json!("2000-01-01T00:00:00Z");
    std::fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
    // The next admission reclaims the capacity whilst the recovery record stays.
    let next = f
        .profile
        .acquire_with_limit(
            "PUBLIC-next",
            1,
            Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let records = f.profile.records().unwrap();
    let kept = records["leases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == retained["id"])
        .cloned()
        .unwrap();
    assert_eq!(kept["capacityReleased"], true);
    assert_eq!(kept["chromeTargetId"], retained["chromeTargetId"]);
    next.release().unwrap();
    // Once the tab is gone from a complete target list, the record protects nothing.
    let lister: crate::browser_profile::TargetLister = Arc::new(|_, _, _| {
        Box::pin(async { crate::browser_profile::EndpointTargets::Listed(Default::default()) })
    });
    assert_eq!(
        f.profile
            .reap_with(
                false,
                Duration::from_secs(5),
                &CancellationToken::new(),
                lister
            )
            .await
            .unwrap(),
        1
    );
    assert!(
        f.profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    stop(f).await;
}
#[tokio::test]
async fn manual_login_sends_wait_for_the_profile_automation_lock() {
    for (manual, held) in [(true, true), (true, false), (false, true)] {
        let f = fixture(Fault::None, false).await;
        f.store
            .update(&f.id, |current| {
                let config = merge_object(
                    &current["browser"]["config"],
                    &json!({"manualLogin":manual}),
                );
                Ok(Some(
                    json!({"browser":merge_object(&current["browser"],&json!({"config":config}))}),
                ))
            })
            .unwrap();
        let path = f.profile.path().join("oracle-automation.lock");
        let lock = held.then(|| DirectoryLock::acquire(&path, Duration::from_secs(1)).unwrap());
        let bounded = RunnerLimits {
            profile_lock: Duration::from_millis(300),
            ..limits()
        };
        let result = execute(
            &f.store,
            &f.id,
            f.profile.clone(),
            f.cdp.clone(),
            f.endpoint.clone(),
            &f.identity,
            &bounded,
            &f.cancel,
        )
        .await;
        if manual && held {
            let error = result.unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "Industrious profile lock still held by pid {} after 0s",
                    std::process::id()
                )
            );
            assert_eq!(f.sends.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(result.unwrap()["status"], "completed");
            assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        }
        if let Some(lock) = lock {
            lock.release().unwrap();
        }
        assert!(!path.exists(), "the run must release its automation lock");
        stop(f).await;
    }
}
#[tokio::test]
async fn held_bringup_lock_times_out_with_the_reference_profile_lock_text() {
    let f = fixture(Fault::None, false).await;
    let held = DirectoryLock::acquire(
        &f.profile.path().join("oracle-browser-bringup.lock"),
        Duration::from_secs(1),
    )
    .unwrap();
    // Reference bring-up waits its own fixed budget, not profileLockTimeoutMs.
    let bounded = RunnerLimits {
        bringup: Duration::from_millis(300),
        ..limits()
    };
    let error = execute(
        &f.store,
        &f.id,
        f.profile.clone(),
        f.cdp.clone(),
        f.endpoint.clone(),
        &f.identity,
        &bounded,
        &f.cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "Industrious profile lock still held by pid {} after 0s",
            std::process::id()
        )
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    held.release().unwrap();
    stop(f).await;
}
#[tokio::test]
async fn cloudflare_challenge_refuses_before_the_claim_and_keeps_the_tab_for_the_check() {
    let f = fixture_attempt(Fault::Cloudflare, false, false, json!({"verbose":true})).await;
    let error = run(&f).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Cloudflare challenge detected. Complete the “Just a moment…” check in the open browser, then rerun."
    );
    assert!(submission_actions(&f).is_empty());
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    assert_eq!(f.closes.load(Ordering::SeqCst), 0);
    let log = f.store.log(&f.id).unwrap();
    let profile = serde_json::to_string(&f.profile.path().display().to_string()).unwrap();
    for line in [
        "Cloudflare anti-bot page detected".to_owned(),
        "Cloudflare challenge detected; leaving browser open so you can complete the check.".into(),
        format!(
            "Reuse this browser profile with: industrious --engine browser --browser-manual-login --browser-manual-login-profile-dir {profile}"
        ),
    ] {
        assert!(log.lines().any(|logged| logged == line), "{line}\n{log}");
    }
    stop(f).await;
}
#[tokio::test]
async fn signed_out_owned_run_refuses_after_one_login_check_and_retires_its_tab() {
    let f = fixture(Fault::SignedOut, false).await;
    let error = run(&f).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "ChatGPT session not detected. Login button detected on page. No ChatGPT cookies were applied; sign in to chatgpt.com in Chrome or pass inline cookies (--browser-inline-cookies[(-file)] / ORACLE_BROWSER_COOKIES_JSON)."
    );
    let probes = f
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|r| {
            r["params"]["expression"]
                .as_str()
                .is_some_and(|e| e.contains("industrious-dom:health-login"))
        })
        .count();
    assert_eq!(probes, 1);
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    let meta = f.store.require(&f.id).unwrap();
    assert!(
        meta["browser"]["runtime"]["targetRetiredAt"].is_string(),
        "{meta}"
    );
    assert!(!submission_actions(&f).iter().any(|a| a == "dispatch"));
    stop(f).await;
}
#[tokio::test]
async fn manual_login_waits_for_the_session_then_reopens_the_landing_page() {
    let f = fixture(Fault::ManualLoginLate, false).await;
    let meta = run(&f).await.unwrap();
    assert_eq!(meta["status"], "completed");
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    let trace = f.trace.lock().unwrap().clone();
    let navigations = trace
        .iter()
        .filter(|r| r["method"] == "Page.navigate")
        .map(|r| r["params"]["url"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        navigations,
        vec![
            json!("https://chatgpt.com/?temporary-chat=true"),
            json!("https://chatgpt.com/?temporary-chat=true")
        ]
    );
    stop(f).await;
}
#[tokio::test]
async fn manual_login_timeout_names_the_private_profile_and_setup_command() {
    let f = fixture(Fault::ManualLoginTimeout, false).await;
    let started = std::time::Instant::now();
    let error = run(&f).await.unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(1500));
    let profile = f.profile.path().display().to_string();
    assert_eq!(
        error.to_string(),
        format!(
            "Manual login mode timed out waiting for ChatGPT session. Browser mode is using Industrious's private Chrome profile at {profile}, not your normal Chrome profile. Run first-time setup, sign in there, then retry: industrious --engine browser --browser-manual-login --browser-keep-browser --browser-manual-login-profile-dir {} -p \"HI\"",
            serde_json::to_string(&profile).unwrap()
        )
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 0);
    stop(f).await;
}
#[tokio::test]
async fn owned_work_mode_is_switched_to_chat_with_one_press_before_the_composer_check() {
    let f = fixture(Fault::OwnedWork, false).await;
    let meta = run(&f).await.unwrap();
    assert_eq!(meta["status"], "completed");
    let trace = f.trace.lock().unwrap().clone();
    let mouse = trace
        .iter()
        .filter(|r| r["method"] == "Input.dispatchMouseEvent")
        .map(|r| {
            (
                r["params"]["type"].as_str().unwrap().to_owned(),
                r["params"]["x"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mouse,
        ["mouseMoved", "mousePressed", "mouseReleased"]
            .map(|kind| (kind.to_owned(), json!(40.5)))
            .to_vec()
    );
    let position = |marker: &str| {
        trace.iter().position(|r| {
            r["params"]["expression"]
                .as_str()
                .is_some_and(|e| e.contains(marker))
        })
    };
    let pressed = trace
        .iter()
        .position(|r| r["params"]["type"] == "mousePressed")
        .unwrap();
    assert!(position("industrious-dom:chat-mode-target").unwrap() < pressed);
    assert!(pressed < position("industrious-dom:navigation").unwrap());
    assert!(pressed < position("industrious-dom:model").unwrap());
    stop(f).await;
}
#[tokio::test]
async fn answer_timeout_reports_a_chatgpt_ui_warning_and_keeps_the_typed_deadline() {
    let f = fixture(Fault::UiWarningTimeout, false).await;
    let error = run(&f).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<crate::browser_provider::Failure>(),
        Some(crate::browser_provider::Failure::AnswerDeadline)
    ));
    assert_eq!(
        error.to_string(),
        "ChatGPT displayed a rate-limit warning while waiting for the assistant: Too many requests. Contact [redacted-email]"
    );
    assert_eq!(
        f.store.require(&f.id).unwrap()["errorMessage"],
        error.to_string()
    );
    stop(f).await;
}
#[tokio::test]
async fn generated_image_wait_stops_on_a_chatgpt_ui_warning() {
    let f = fixture(Fault::ImageUiWarning, false).await;
    let error = run(&f).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "ChatGPT displayed a rate-limit warning while waiting for generated image artifacts: Too many requests. Contact [redacted-email]"
    );
    let log = f.store.log(&f.id).unwrap();
    assert!(
        !log.contains("PUBLIC@example.com"),
        "UI warning text must be sanitised: {log}"
    );
    stop(f).await;
}

#[tokio::test]
async fn stale_conversation_cookies_are_cleared_on_the_own_target_before_navigation() {
    // Reference logger lines starting with [cookies] print in verbose runs only.
    let f = fixture_attempt(
        Fault::StaleConversationCookie,
        false,
        false,
        json!({"verbose":true}),
    )
    .await;
    run(&f).await.unwrap();
    let methods: Vec<String> = f
        .trace
        .lock()
        .unwrap()
        .iter()
        .map(|row| row["method"].as_str().unwrap().to_owned())
        .collect();
    let position = |method: &str| methods.iter().position(|row| row == method).unwrap();
    assert!(position("Network.getAllCookies") < position("Network.deleteCookies"));
    assert!(position("Network.deleteCookies") < position("Page.navigate"));
    assert_eq!(
        methods
            .iter()
            .filter(|row| *row == "Network.deleteCookies")
            .count(),
        1
    );
    let log = f.store.log(&f.id).unwrap();
    assert!(
        log.contains("[cookies] Cleared 1 stale ChatGPT conversation cookie.\n"),
        "{log}"
    );
    assert!(!log.contains("PUBLIC-cookie-value-sentinel"));
    stop(f).await;
}

/// Loopback-only DevTools discovery: `/json/version` answers, and `/json/list` returns `targets`.
async fn fake_discovery(targets: Value) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut request = vec![0; 1024];
            let read = socket.read(&mut request).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            let body = if request.starts_with("GET /json/version ") {
                json!({"Browser":"PUBLIC"})
            } else if request.starts_with("GET /json/list ") {
                targets.clone()
            } else {
                panic!("Unaccounted discovery request {request}");
            }
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (base, server)
}

#[tokio::test]
async fn lost_devtools_transport_reports_the_probed_cause_and_never_replays_send() {
    let cases = [
        (
            json!([{"id":"TARGET-PUBLIC","type":"page"}]),
            "Chrome DevTools client disconnected before Industrious finished; the browser target appears still alive.",
            "cdp-client-disconnect",
            "CDP client disconnected; Chrome/target still reachable. Leaving run recoverable for reattach.",
        ),
        (
            json!([{"id":"FOREIGN-PUBLIC","type":"page"}]),
            "Chrome closed the ChatGPT tab before Industrious finished; Chrome itself is still running.",
            "target-closed",
            "Chrome connection lost (target-closed); attempting to abort run.",
        ),
    ];
    for (targets, message, cause, line) in cases {
        let (base, server) = fake_discovery(targets).await;
        let f = fixture_attempt(Fault::LostSendAck, false, false, json!({"verbose":true})).await;
        let error = crate::browser::TEST_DISCOVERY
            .scope(base, run(&f))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), message);
        // The transport failure stays in the chain behind the reference wording.
        assert!(format!("{error:#}").starts_with(&format!("{message}: ")));
        let saved = f.store.require(&f.id).unwrap();
        assert_eq!(saved["status"], "error");
        assert_eq!(saved["errorMessage"], message);
        assert_eq!(saved["recoveryRequired"], true);
        assert_eq!(saved["dispatch"]["state"], "uncertain");
        assert_eq!(
            saved["error"],
            json!({"category":"browser-automation","message":message,"details":{"stage":"connection-lost","recoverableDisconnect":cause=="cdp-client-disconnect","disconnectCause":cause}})
        );
        let log = f.store.log(&f.id).unwrap();
        assert!(log.lines().any(|row| row == line), "{log}");
        assert_eq!(f.sends.load(Ordering::SeqCst), 1);
        stop(f).await;
        server.abort();
    }
}

#[tokio::test]
async fn unreachable_devtools_after_a_lost_transport_reads_as_a_closed_window() {
    let (base, server) = fake_discovery(json!([])).await;
    server.abort();
    let _ = server.await;
    let f = fixture(Fault::LostSendAck, false).await;
    let error = crate::browser::TEST_DISCOVERY
        .scope(base, run(&f))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Chrome window closed before Industrious finished. Please keep it open until completion."
    );
    assert_eq!(
        f.store.require(&f.id).unwrap()["error"]["details"]["disconnectCause"],
        "chrome-closed"
    );
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    stop(f).await;
}

/// A documentation-range host: the fixture CDP stays in memory, so nothing reaches it.
fn remote_endpoint() -> AttachmentEndpoint {
    AttachmentEndpoint {
        host: "192.0.2.1".into(),
        port: 9222,
        browser_ws_endpoint: "ws://192.0.2.1:9222/devtools/browser/PUBLIC".into(),
    }
}

#[tokio::test]
async fn remote_host_uploads_through_the_page_and_never_sets_local_paths() {
    let mut f = fixture_attempt(Fault::None, true, false, json!({"verbose":true})).await;
    f.endpoint = remote_endpoint();
    run(&f).await.unwrap();
    let trace = f.trace.lock().unwrap().clone();
    let expressions = |marker: &str| {
        trace
            .iter()
            .filter(|row| {
                row["params"]["expression"]
                    .as_str()
                    .or(row["params"]["functionDeclaration"].as_str())
                    .is_some_and(|e| e.contains(marker))
            })
            .count()
    };
    assert!(trace.iter().all(|row| !matches!(
        row["method"].as_str(),
        Some("DOM.setFileInputFiles" | "DOM.describeNode")
    )));
    assert_eq!(expressions("industrious-dom:data-transfer-chunk"), 1);
    assert_eq!(expressions("industrious-dom:data-transfer-assign"), 1);
    assert_eq!(expressions("industrious-dom:file-input-seal"), 1);
    let log = f.store.log(&f.id).unwrap();
    for line in [
        "Uploading attachment: public.txt",
        "Transferring public.txt to remote browser...",
        "File transferred: public.txt (20 bytes)",
        "Attachment queued",
    ] {
        assert!(log.lines().any(|row| row == line), "{line}\n{log}");
    }
    assert_eq!(f.sends.load(Ordering::SeqCst), 1);
    stop(f).await;
}

#[tokio::test]
async fn remote_host_downloads_are_not_redirected_into_local_directories() {
    // The reference automation filter prints this warning in verbose runs only.
    let mut f = fixture_attempt(Fault::File, false, false, json!({"verbose":true})).await;
    f.endpoint = remote_endpoint();
    run(&f).await.unwrap();
    assert!(
        f.trace
            .lock()
            .unwrap()
            .iter()
            .all(|row| row["method"] != "Browser.setDownloadBehavior")
    );
    let meta = f.store.require(&f.id).unwrap();
    assert!(
        meta["artifacts"]
            .as_array()
            .is_none_or(|rows| rows.iter().all(|row| row["kind"] != "file"))
    );
    let log = f.store.log(&f.id).unwrap();
    assert!(log.lines().any(|row| row == "[browser] WARNING: 1 downloadable candidate(s) existed, but no local browser-host artifact was saved; bridge artifact-ready will not be emitted until ChatGPT file capture succeeds."), "{log}");
    stop(f).await;
}
