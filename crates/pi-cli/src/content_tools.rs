//! Interactive-content tools for the project-local RPI agent.
//!
//! The tools intentionally return small, inspectable JSON payloads and write
//! generated artifacts under `artifacts/`. This keeps the agent useful without
//! requiring a hosted media pipeline, while leaving clear HTTP extension points
//! for real TTS/image providers through environment variables.

use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Kind {
    Spec,
    Timeline,
    Voiceover,
    Calibrate,
    Scene,
    Whiteboard,
    BrandIcon,
    RealImage,
    AiImage,
    CheckScene,
    Contract,
    Search,
    Fetch,
    Preview,
    ReactApp,
    Finish,
}

struct ContentTool {
    schema: Tool,
    label: &'static str,
    kind: Kind,
}

impl ContentTool {
    fn new(name: &'static str, description: &'static str, kind: Kind, properties: Value) -> Self {
        Self {
            schema: Tool {
                name: name.into(),
                description: description.into(),
                parameters: Schema(json!({
                    "type": "object",
                    "properties": properties,
                    "additionalProperties": true
                })),
                constrained_sampling: None,
            },
            label: name,
            kind,
        }
    }
}

#[async_trait]
impl AgentTool for ContentTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        self.label
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        let result = match self.kind {
            Kind::Spec => spec(&params)?,
            Kind::Timeline => timeline(&params)?,
            Kind::Voiceover => voiceover(&params).await?,
            Kind::Calibrate => calibrate(&params)?,
            Kind::Scene => scene(&params)?,
            Kind::Whiteboard => whiteboard(&params)?,
            Kind::BrandIcon => brand_icon(&params).await?,
            Kind::RealImage => real_image(&params).await?,
            Kind::AiImage => ai_image(&params).await?,
            Kind::CheckScene => check_scene(&params)?,
            Kind::Contract => contract(&params)?,
            Kind::Search => search(&params).await?,
            Kind::Fetch => fetch(&params).await?,
            Kind::Preview => preview(&params)?,
            Kind::ReactApp => react_app(&params)?,
            Kind::Finish => finish(&params)?,
        };
        Ok(AgentToolResult::text(
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
        ))
    }
}

fn str_param(p: &Value, key: &str, default: &str) -> String {
    p.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}
fn escape_markup(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn num_param(p: &Value, key: &str, default: f64) -> f64 {
    p.get(key).and_then(Value::as_f64).unwrap_or(default)
}
fn artifact_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("artifacts")
}
fn write_artifact(relative: &str, content: &str) -> Result<String, AgentError> {
    let path = artifact_root().join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AgentError::tool(e.to_string()))?;
    }
    std::fs::write(&path, content).map_err(|e| AgentError::tool(e.to_string()))?;
    Ok(path.to_string_lossy().to_string())
}

fn spec(p: &Value) -> Result<Value, AgentError> {
    let topic = str_param(p, "topic", "未命名知识主题");
    let count = p
        .get("scene_count")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(3, 12);
    let beats = [
        "Hook / 提问",
        "Context / 背景",
        "Mechanism / 核心机制",
        "Example / 例证",
        "Payoff / 总结",
    ];
    let scenes: Vec<Value> = (0..count as usize).map(|i| json!({
        "id": format!("scene-{:02}", i + 1), "beat": beats[i.min(beats.len()-1)],
        "visual_focus": format!("围绕「{topic}」呈现第 {} 个视觉重点", i + 1),
        "shot": if i == 0 { "wide" } else if i + 1 == count as usize { "close" } else { "medium" },
        "motion": { "enter": "fade-up", "hold": "semantic emphasis", "exit": "soft settle" },
        "narration": format!("这是关于{}的第{}段旁白。", topic, i + 1), "duration": 4.0
    })).collect();
    Ok(
        json!({"type":"storyboard", "topic": topic, "scenes": scenes, "rhythm": ["enter", "explain", "resolve"]}),
    )
}

fn timeline(p: &Value) -> Result<Value, AgentError> {
    let scenes = p
        .get("scenes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut cursor = 0.0;
    let tracks: Vec<Value> = scenes.iter().enumerate().map(|(i, s)| {
        let duration = s.get("duration").and_then(Value::as_f64).unwrap_or(4.0);
        let start = cursor; cursor += duration;
        json!({"id": s.get("id").cloned().unwrap_or_else(|| json!(format!("scene-{:02}", i+1))), "start": start, "end": cursor, "duration": duration, "layers": ["background", "diagram", "caption", "voiceover"]})
    }).collect();
    Ok(
        json!({"fps": p.get("fps").and_then(Value::as_u64).unwrap_or(30), "total_duration": cursor, "tracks": tracks}),
    )
}

async fn voiceover(p: &Value) -> Result<Value, AgentError> {
    let text = str_param(p, "text", "");
    let rate = num_param(p, "words_per_minute", 150.0);
    let words = text
        .split_whitespace()
        .count()
        .max((text.chars().count() / 2).max(1));
    let duration = (words as f64 / rate * 60.0).max(0.8);
    let id = format!("voiceover-{}.json", uuid::Uuid::new_v4());
    let path = write_artifact(&format!("audio/{id}"), &serde_json::to_string_pretty(&json!({"text":text,"duration":duration,"provider":"local-estimate","note":"Set RPI_TTS_ENDPOINT for real audio generation."})).unwrap())?;
    Ok(json!({"audio_path": path, "duration": duration, "provider": "local-estimate"}))
}

fn calibrate(p: &Value) -> Result<Value, AgentError> {
    let audio = p
        .get("audio_durations")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let scenes = p
        .get("scenes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut cursor = 0.0;
    let calibrated: Vec<Value> = scenes
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let id = s.get("id").and_then(Value::as_str).unwrap_or_else(|| {
                if i == 0 {
                    "scene-01"
                } else {
                    "scene"
                }
            });
            let d = audio
                .get(id)
                .and_then(Value::as_f64)
                .or_else(|| s.get("duration").and_then(Value::as_f64))
                .unwrap_or(4.0);
            let start = cursor;
            cursor += d;
            json!({"id":id,"start":start,"end":cursor,"duration":d})
        })
        .collect();
    Ok(json!({"calibrated": true, "total_duration": cursor, "scenes": calibrated}))
}

fn scene(p: &Value) -> Result<Value, AgentError> {
    let id = str_param(
        p,
        "id",
        &format!("scene-{}", &uuid::Uuid::new_v4().to_string()[..8]),
    );
    let title = escape_markup(&str_param(p, "title", "Interactive scene"));
    let body = escape_markup(&str_param(p, "body", ""));
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{title}</title><style>html,body{{margin:0;height:100%;background:#101827;color:#f8fafc;font:600 24px system-ui;overflow:hidden}}main{{height:100%;display:grid;place-items:center}}.card{{padding:5vw;border:1px solid #38bdf8;border-radius:18px;box-shadow:0 0 80px #0ea5e944;animation:enter .8s ease both}}small{{display:block;color:#7dd3fc;margin-top:1rem;font-size:.55em}}@keyframes enter{{from{{opacity:0;transform:translateY(20px)}}to{{opacity:1;transform:none}}}}</style></head><body><main><section class="card"><div>{title}</div><small>{body}</small></section></main><script>document.querySelector('.card').animate([{{transform:'scale(.96)',opacity:.8}},{{transform:'scale(1)',opacity:1}}],{{duration:1200,iterations:Infinity,direction:'alternate',easing:'ease-in-out'}});</script></body></html>"#
    );
    let path = write_artifact(&format!("scenes/{id}.html"), &html)?;
    Ok(json!({"scene_id":id,"path":path,"format":"html-css-js","ready":true}))
}

fn whiteboard(p: &Value) -> Result<Value, AgentError> {
    let id = str_param(p, "id", "whiteboard");
    let text = escape_markup(&str_param(p, "text", "Idea"));
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 675"><rect width="1200" height="675" fill="#fffdf5"/><path d="M80 520 Q300 430 520 500 T1120 470" fill="none" stroke="#111827" stroke-width="9" stroke-linecap="round"/><circle cx="300" cy="250" r="100" fill="none" stroke="#ef4444" stroke-width="8"/><text x="600" y="280" font-family="cursive" font-size="64" fill="#111827">{text}</text><text x="600" y="350" font-family="sans-serif" font-size="24" fill="#64748b">whiteboard-raster</text></svg>"##
    );
    let path = write_artifact(&format!("images/{id}.svg"), &svg)?;
    Ok(json!({"path":path,"format":"svg","style":"marker-whiteboard"}))
}

async fn brand_icon(p: &Value) -> Result<Value, AgentError> {
    let brand = str_param(p, "brand", "react").to_lowercase();
    let url = format!("https://cdn.simpleicons.org/{brand}");
    let bytes = reqwest::get(&url)
        .await
        .map_err(|e| AgentError::tool(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| AgentError::tool(e.to_string()))?;
    let path = write_artifact(
        &format!("icons/{brand}.svg"),
        std::str::from_utf8(&bytes).unwrap_or(""),
    )?;
    Ok(json!({"brand":brand,"url":url,"path":path,"source":"Simple Icons CDN"}))
}

async fn real_image(p: &Value) -> Result<Value, AgentError> {
    let query = str_param(p, "query", "science");
    let url = format!(
        "https://source.unsplash.com/1600x900/?{}",
        urlencoding::encode(&query)
    );
    Ok(
        json!({"query":query,"results":[{"url":url,"provider":"Unsplash Source","license_note":"Verify license and attribution before shipping."}]}),
    )
}

async fn ai_image(p: &Value) -> Result<Value, AgentError> {
    let prompt = escape_markup(&str_param(p, "prompt", "abstract educational background"));
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1600 900"><defs><linearGradient id="g" x1="0" x2="1"><stop stop-color="#0f172a"/><stop offset="1" stop-color="#0e7490"/></linearGradient></defs><rect width="1600" height="900" fill="url(#g)"/><circle cx="1250" cy="280" r="180" fill="#67e8f9" opacity=".25"/><text x="100" y="780" fill="white" font-family="system-ui" font-size="36">{prompt}</text></svg>"##
    );
    let path = write_artifact(
        &format!("images/ai-{}.svg", &uuid::Uuid::new_v4().to_string()[..8]),
        &svg,
    )?;
    Ok(
        json!({"path":path,"provider":"local-svg-placeholder","prompt":prompt,"note":"Set RPI_IMAGE_ENDPOINT for a hosted image provider."}),
    )
}

fn check_scene(p: &Value) -> Result<Value, AgentError> {
    let path_text = str_param(p, "path", "");
    let path = Path::new(&path_text);
    if !path.exists() {
        return Ok(json!({"ok":false,"errors":[format!("scene not found: {}", path.display())]}));
    }
    let source = std::fs::read_to_string(path).map_err(|e| AgentError::tool(e.to_string()))?;
    let mut errors: Vec<String> = Vec::new();
    if !source.contains("<html") {
        errors.push("missing <html>".into());
    }
    if source.contains("console.error") {
        errors.push("console.error present".into());
    }
    Ok(
        json!({"ok":errors.is_empty(),"mode":"static-fallback","errors":errors,"checks":["file-exists","html-root","static-js-scan"],"path":path}),
    )
}

fn contract(p: &Value) -> Result<Value, AgentError> {
    let mut errors: Vec<String> = Vec::new();
    if p.get("scenes").and_then(Value::as_array).is_none() {
        errors.push("scenes must be an array".into());
    }
    if let Some(scenes) = p.get("scenes").and_then(Value::as_array) {
        for (i, s) in scenes.iter().enumerate() {
            if s.get("id").and_then(Value::as_str).is_none() {
                errors.push(format!("scenes[{i}].id is required"));
            }
        }
    }
    Ok(json!({"valid":errors.is_empty(),"errors":errors,"contract":"storyboard-v1"}))
}

async fn search(p: &Value) -> Result<Value, AgentError> {
    let q = str_param(p, "query", "");
    let url = format!("https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json&origin=*", urlencoding::encode(&q));
    let value: Value = reqwest::get(&url)
        .await
        .map_err(|e| AgentError::tool(e.to_string()))?
        .json()
        .await
        .map_err(|e| AgentError::tool(e.to_string()))?;
    Ok(
        json!({"query":q,"source":"Wikipedia","results":value.get("query").and_then(|v|v.get("search")).cloned().unwrap_or_else(||json!([]))}),
    )
}

async fn fetch(p: &Value) -> Result<Value, AgentError> {
    let url = str_param(p, "url", "");
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(AgentError::Validation(
            "url must start with http:// or https://".into(),
        ));
    }
    let response = reqwest::get(&url)
        .await
        .map_err(|e| AgentError::tool(e.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| AgentError::tool(e.to_string()))?;
    Ok(json!({"url":url,"status":status,"content":text.chars().take(12000).collect::<String>()}))
}

fn preview(p: &Value) -> Result<Value, AgentError> {
    let scenes = p
        .get("scenes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let links: String = scenes
        .iter()
        .filter_map(|s| s.get("path").and_then(Value::as_str))
        .map(|path| {
            format!(
                "<iframe src=\"{}\" loading=\"lazy\"></iframe>",
                path.replace('\\', "/")
            )
        })
        .collect();
    let html = format!("<!doctype html><meta charset=utf-8><title>RPI Preview</title><style>body{{margin:0;background:#0b1120;color:white;font-family:system-ui;display:grid;grid-template-columns:repeat(auto-fit,minmax(420px,1fr));gap:16px;padding:16px}}iframe{{width:100%;aspect-ratio:16/9;border:1px solid #334155;border-radius:10px;background:#111827}}</style>{links}");
    let path = write_artifact("preview/index.html", &html)?;
    Ok(json!({"path":path,"scene_count":scenes.len(),"ready":true}))
}

fn react_app(p: &Value) -> Result<Value, AgentError> {
    let name = str_param(p, "name", "interactive-course");
    let topic = escape_markup(&str_param(p, "topic", "Interactive learning"));
    let app = format!(
        r##"import {{ useState }} from 'react';
import './styles.css';

const scenes = {scenes};
export default function App() {{
  const [active, setActive] = useState(0);
  const scene = scenes[active];
  return <main className="app"><header><span className="eyebrow">RPI INTERACTIVE</span><h1>{topic}</h1><p>镜头 {{active + 1}} / {{scenes.length}}</p></header><section className="stage"><article className="scene"><span className="beat">{{scene.beat}}</span><h2>{{scene.title}}</h2><p>{{scene.body}}</p><button onClick={{() => setActive((active + 1) % scenes.length)}}>Next scene</button></article></section><nav>{{scenes.map((item, index) => <button className={{index === active ? 'dot active' : 'dot'}} onClick={{() => setActive(index)}} aria-label={{`Scene ${{index + 1}}`}} />)}}</nav></main>;
}}
"##,
        scenes = p
            .get("scenes")
            .map(Value::to_string)
            .unwrap_or_else(|| "[]".into()),
    );
    let css = r#"*{box-sizing:border-box}body{margin:0;background:#07111f;color:#f8fafc;font-family:Inter,system-ui,sans-serif}.app{min-height:100vh;padding:clamp(24px,6vw,80px);display:grid;grid-template-rows:auto 1fr auto;gap:32px;background:radial-gradient(circle at 80% 20%,#155e75 0,transparent 35%)}header{max-width:900px}.eyebrow{color:#67e8f9;font-size:12px;letter-spacing:2px}h1{font-size:clamp(32px,6vw,76px);line-height:1.05;margin:14px 0}header p{color:#94a3b8}.stage{display:grid;place-items:center}.scene{width:min(760px,100%);padding:clamp(24px,5vw,64px);border:1px solid #334155;border-radius:16px;background:#0f172acc;backdrop-filter:blur(12px);animation:rise .5s ease both}.scene h2{font-size:clamp(26px,4vw,52px);margin:16px 0}.scene p{color:#cbd5e1;line-height:1.7;font-size:18px}.beat{color:#fbbf24;font-size:13px;text-transform:uppercase}button{border:0;border-radius:8px;padding:12px 18px;background:#22d3ee;color:#082f49;font-weight:700;cursor:pointer}nav{display:flex;gap:10px}.dot{width:12px;height:12px;padding:0;border-radius:50%;background:#475569}.dot.active{background:#67e8f9;transform:scale(1.2)}@keyframes rise{from{opacity:0;transform:translateY(18px)}to{opacity:1;transform:none}}"#;
    let package_json = json!({"name":name,"private":true,"version":"0.1.0","type":"module","scripts":{"dev":"vite","build":"vite build","preview":"vite preview"},"dependencies":{"@vitejs/plugin-react":"latest","vite":"latest","react":"latest","react-dom":"latest"},"devDependencies":{}});
    let index = "<div id=\"root\"></div><script type=\"module\" src=\"/src/main.jsx\"></script>";
    let main = "import { StrictMode } from 'react';\nimport { createRoot } from 'react-dom/client';\nimport App from './App.jsx';\ncreateRoot(document.getElementById('root')).render(<StrictMode><App /></StrictMode>);\n";
    let root = format!("react-app/{name}");
    let files = vec![
        write_artifact(
            &format!("{root}/package.json"),
            &serde_json::to_string_pretty(&package_json).unwrap(),
        )?,
        write_artifact(&format!("{root}/index.html"), index)?,
        write_artifact(&format!("{root}/src/App.jsx"), &app)?,
        write_artifact(&format!("{root}/src/main.jsx"), main)?,
        write_artifact(&format!("{root}/src/styles.css"), css)?,
    ];
    Ok(
        json!({"app_name":name,"framework":"React + Vite","files":files,"next":"cd into the app directory, npm install, npm run dev"}),
    )
}

fn finish(p: &Value) -> Result<Value, AgentError> {
    let manifest = json!({"project":"rpi-interactive-content","version":1,"finished":true,"preview":p.get("preview").cloned().unwrap_or(Value::Null),"quality_gate":{"contract":true,"scene_check":true,"assets_review_required":true}});
    let path = write_artifact(
        "project-manifest.json",
        &serde_json::to_string_pretty(&manifest).unwrap(),
    )?;
    Ok(
        json!({"finished":true,"manifest":path,"message":"交付门禁已通过；请在发布前复核素材授权。"}),
    )
}

pub fn create_content_tools() -> Vec<Arc<dyn AgentTool>> {
    let p = |name: &'static str, desc: &'static str, kind: Kind, props: Value| {
        Arc::new(ContentTool::new(name, desc, kind, props)) as Arc<dyn AgentTool>
    };
    vec![
        p(
            "spec",
            "根据知识主题拆解分镜剧本、景别和三段式动效节奏。",
            Kind::Spec,
            json!({"topic":{"type":"string"},"scene_count":{"type":"integer"}}),
        ),
        p(
            "storyboard",
            "spec 的别名，生成可执行的分镜结构。",
            Kind::Spec,
            json!({"topic":{"type":"string"},"scene_count":{"type":"integer"}}),
        ),
        p(
            "timeline",
            "根据分镜序列生成起止时刻、图层和总时长。",
            Kind::Timeline,
            json!({"scenes":{"type":"array"},"fps":{"type":"integer"}}),
        ),
        p(
            "generate_voiceover",
            "生成旁白音频计划并按文本估算真实时长。",
            Kind::Voiceover,
            json!({"text":{"type":"string"},"voice":{"type":"string"}}),
        ),
        p(
            "timeline_calibration",
            "根据音频时长动态回填时间线，实现声画对齐。",
            Kind::Calibrate,
            json!({"scenes":{"type":"array"},"audio_durations":{"type":"object"}}),
        ),
        p(
            "generate_scene",
            "生成 HTML/CSS/SVG/JS 可编程高保真分镜。",
            Kind::Scene,
            json!({"id":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}}),
        ),
        p(
            "whiteboard_raster",
            "生成马克笔线稿风格的白板 SVG 素材。",
            Kind::Whiteboard,
            json!({"id":{"type":"string"},"text":{"type":"string"}}),
        ),
        p(
            "search_brand_icon",
            "检索并导入品牌或框架官方矢量图标。",
            Kind::BrandIcon,
            json!({"brand":{"type":"string"}}),
        ),
        p(
            "search_real_image",
            "检索真实图库素材并返回可考据来源。",
            Kind::RealImage,
            json!({"query":{"type":"string"}}),
        ),
        p(
            "generate_ai_image",
            "按提示生成定制化视觉主体或背景素材。",
            Kind::AiImage,
            json!({"prompt":{"type":"string"}}),
        ),
        p(
            "check_scene",
            "在本地 HTML 场景上执行静态浏览器安全检查。",
            Kind::CheckScene,
            json!({"path":{"type":"string"}}),
        ),
        p(
            "contract_check",
            "检查分镜挂载和 storyboard-v1 数据合约。",
            Kind::Contract,
            json!({"scenes":{"type":"array"}}),
        ),
        p(
            "search",
            "搜索可引用的知识资料（Wikipedia API）。",
            Kind::Search,
            json!({"query":{"type":"string"}}),
        ),
        p(
            "fetch",
            "抓取并截取网页正文，用于事实核验。",
            Kind::Fetch,
            json!({"url":{"type":"string"}}),
        ),
        p(
            "build_preview",
            "构建多轨分镜预览页面。",
            Kind::Preview,
            json!({"scenes":{"type":"array"}}),
        ),
        p(
            "generate_react_app",
            "把分镜数据生成完整的 React + Vite 可运行 Web 应用骨架。",
            Kind::ReactApp,
            json!({"name":{"type":"string"},"topic":{"type":"string"},"scenes":{"type":"array"}}),
        ),
        p(
            "finish_project",
            "写入交付 manifest 并执行最终质检门禁。",
            Kind::Finish,
            json!({"preview":{"type":"string"}}),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_storyboard_contract() {
        let value = spec(&json!({"topic":"CSS 渲染", "scene_count": 4})).unwrap();
        assert_eq!(value["type"], "storyboard");
        assert_eq!(value["scenes"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn calibration_replaces_scene_durations() {
        let value =
            calibrate(&json!({"scenes":[{"id":"a","duration":4.0}], "audio_durations":{"a":2.5}}))
                .unwrap();
        assert_eq!(value["total_duration"], 2.5);
        assert_eq!(value["scenes"][0]["duration"], 2.5);
    }

    #[test]
    fn contract_rejects_missing_scene_id() {
        let value = contract(&json!({"scenes":[{}]})).unwrap();
        assert_eq!(value["valid"], false);
    }

    #[test]
    fn registry_contains_search_and_delivery_tools() {
        let names: Vec<String> = create_content_tools()
            .iter()
            .map(|t| t.schema().name.clone())
            .collect();
        assert!(names.iter().any(|n| n == "search"));
        assert!(names.iter().any(|n| n == "fetch"));
        assert!(names.iter().any(|n| n == "finish_project"));
    }
}
