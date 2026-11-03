import type {
  CancelRunResponse,
  InterventionAnswerRequest,
  InterventionAnswerResponse,
  InterventionListResponse,
  CreateSessionRequest,
  DeleteSessionRequest,
  EventPage,
  FileDiff,
  ForkSessionResponse,
  ModelsResponse,
  NameSessionRequest,
  PurgeSessionResponse,
  RunChanges,
  SessionChanges,
  SessionLifecycleResponse,
  SessionSummary,
  SubmitRunRequest,
  SubmitRunResponse,
  UploadAttachmentResponse,
} from "@komo/protocol";
import { get, post, request } from "./http";
import type { SessionTree } from "./types";

// 读
export const fetchHome = () => get<SessionSummary>("/v1/home-session");
export const fetchModels = () => get<ModelsResponse>("/v1/models");
export const fetchInterventions = () => get<InterventionListResponse>("/v1/interventions");
export const fetchEventPage = (session: string, from: number, signal?: AbortSignal) =>
  get<EventPage>(`/v1/sessions/${enc(session)}/events`, { from, limit: 500 }, signal);
export const fetchTree = (session: string) => get<SessionTree>(`/v1/sessions/${enc(session)}/tree`);
export const fetchSessionChanges = (session: string) => get<SessionChanges>(`/v1/sessions/${enc(session)}/changes`);
export const fetchRunChanges = (run: string) => get<RunChanges>(`/v1/runs/${enc(run)}/changes`);
export const fetchRunDiff = (run: string, index: number) => get<FileDiff>(`/v1/runs/${enc(run)}/changes/${index}`);

// 写
export const createSession = (body: CreateSessionRequest) => post<SessionSummary>("/v1/sessions", body);
export const forkSession = (session: string) => post<ForkSessionResponse>(`/v1/sessions/${enc(session)}/fork`);
export const renameSession = (session: string, name: string) =>
  post<unknown>(`/v1/sessions/${enc(session)}/name`, { name } satisfies NameSessionRequest);
/** 归档：`/delete`（逻辑删除，内容还在）。`now` 立刻取消没跑完的 Run。 */
export const archiveSession = (session: string, body: DeleteSessionRequest = {}) =>
  post<SessionLifecycleResponse>(`/v1/sessions/${enc(session)}/delete`, body);
/** 永久删内容。Gateway 只认已归档的会话。 */
export const purgeSession = (session: string) => post<PurgeSessionResponse>(`/v1/sessions/${enc(session)}/purge`);
export const submitRun = (session: string, body: SubmitRunRequest) =>
  post<SubmitRunResponse>(`/v1/sessions/${enc(session)}/runs`, body);
/** 存一个附件（粘贴、拖入或选的图片与文件），回执里的 `note` 拼在正文前面一起提交（Gateway 不认识附件）。 */
export const uploadAttachment = (file: Blob, name?: string, signal?: AbortSignal) =>
  request<UploadAttachmentResponse>("POST", "/v1/attachments", { raw: file, query: { name }, signal });
/** 答复一条待处理；同一请求键重发返回原结论。 */
export const answerIntervention = (handle: string, body: InterventionAnswerRequest) =>
  post<InterventionAnswerResponse>(`/v1/interventions/${enc(handle)}/answer`, body);
export const cancelRun = (run: string) => post<CancelRunResponse>(`/v1/runs/${enc(run)}/cancel`);

const enc = encodeURIComponent;
