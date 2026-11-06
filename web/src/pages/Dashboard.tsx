import { apiGet, type Quota, type Settings, type Status } from "../api";
import { ErrorBox, Loading, pct, useFetch } from "../ui";

export default function Dashboard() {
  const status = useFetch(() => apiGet<Status>("/status"));
  const quota = useFetch(() => apiGet<Quota>("/quota"));
  const settings = useFetch(() => apiGet<Settings>("/settings"));

  if (status.loading || quota.loading || settings.loading) return <Loading />;
  if (status.error) return <ErrorBox message={status.error} />;
  const s = status.data!;
  const q = quota.data;
  const aux = settings.data?.aux;
  const pacing = s.pacing;
  const halted = s.lanes.filter((lane) => lane.halted);

  return (
    <div className="grid">
      <section className="card" aria-labelledby="d-ready">
        <h2 id="d-ready">运行状态</h2>
        <p className={s.health.production_ready ? "ok" : "bad"}>
          {s.health.production_ready ? "生产就绪" : "未就绪"}
        </p>
        {s.health.blockers.length > 0 && (
          <ul aria-label="未就绪原因">
            {s.health.blockers.map((b) => (
              <li key={b}>{b}</li>
            ))}
          </ul>
        )}
        <dl>
          <div>
            <dt>机器人</dt>
            <dd>{s.bot_id}</dd>
          </div>
          <div>
            <dt>群数量</dt>
            <dd>{s.groups}</dd>
          </div>
          <div>
            <dt>待发消息</dt>
            <dd>{s.pending_messages}</dd>
          </div>
          <div>
            <dt>代理调用总数</dt>
            <dd>{s.calls_total}</dd>
          </div>
        </dl>
        {halted.length > 0 && (
          <p role="alert" className="bad">
            通道已停：{halted.map((lane) => lane.group_id).join("、")}
          </p>
        )}
      </section>

      <section className="card" aria-labelledby="d-quota">
        <h2 id="d-quota">额度与节奏</h2>
        {q && (
          <dl>
            <div>
              <dt>周额度剩余</dt>
              <dd>
                {pct(q.gemini.weekly?.remaining_fraction)}
                {q.gemini.weekly?.reset_time &&
                  `（重置 ${new Date(q.gemini.weekly.reset_time).toLocaleString("zh-CN", { hour12: false })}）`}
              </dd>
            </div>
            <div>
              <dt>5 小时额度剩余</dt>
              <dd>
                {pct(q.gemini.five_hour?.remaining_fraction)}
                {q.gemini.five_hour?.reset_time &&
                  `（重置 ${new Date(q.gemini.five_hour.reset_time).toLocaleString("zh-CN", { hour12: false })}）`}
              </dd>
            </div>
            {q.error && (
              <div>
                <dt>抓取错误</dt>
                <dd className="bad">{q.error}</dd>
              </div>
            )}
          </dl>
        )}
        <dl>
          <div>
            <dt>当前档位</dt>
            <dd>
              {pacing.tier}
              {pacing.raw_tier !== pacing.tier && `（原始 ${pacing.raw_tier}）`}
              {pacing.stale && "（快照已过期）"}
            </dd>
          </div>
          <div>
            <dt>额度降级</dt>
            <dd>{pacing.degradation_enabled ? "开" : "关（仅显示）"}</dd>
          </div>
        </dl>
      </section>

      <section className="card" aria-labelledby="d-aux">
        <h2 id="d-aux">辅助模型（aux）</h2>
        {aux && (
          <dl>
            <div>
              <dt>当前模式</dt>
              <dd>{aux.mode === "gateway" ? "网关模式" : "agy 模式（独立辅助会话）"}</dd>
            </div>
            <div>
              <dt>网关</dt>
              <dd>
                {aux.enabled ? `${aux.base_url} · ${aux.model}` : "未启用"}
              </dd>
            </div>
            <div>
              <dt>健康</dt>
              <dd>
                {s.health.aux?.ready
                  ? "正常"
                  : (s.health.aux?.error ?? "不可用")}
              </dd>
            </div>
          </dl>
        )}
        <p className="muted">在「设置」页切换模式与网关参数，立即生效。</p>
      </section>

      <section className="card" aria-labelledby="d-outbox">
        <h2 id="d-outbox">发件箱</h2>
        {s.outbox.length === 0 ? (
          <p className="muted">无积压。</p>
        ) : (
          <ul>
            {s.outbox.map((row) => (
              <li key={row.state}>
                {row.state}：{row.count}
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
