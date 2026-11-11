const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-alice";
const POSITION: &str = "v1.AAAAAAAAAAEAAAAAAAAAKg";

fn consumer(name: &str) -> ConsumerOut {
    ConsumerOut {
        name: name.to_owned(),
        owner: "0192f3c4-0000-7000-8000-00000000a001".to_owned(),
        topics: vec!["doc_posted".to_owned()],
        filter: None,
        partitions: 2,
        payload_mode: PayloadModeIn::IdsOnly,
        partitions_state: vec![
            PartitionOut {
                partition: 0,
                checkpoint: POSITION.to_owned(),
                parked: false,
            },
            PartitionOut {
                partition: 1,
                checkpoint: POSITION.to_owned(),
                parked: true,
            },
        ],
    }
}

/// What the fake saw, and what it answers next.
#[derive(Default)]
struct Script {
    /// The next call fails with this.
    fail: Option<ApiError>,
    /// `put` answers "created" when true.
    created: bool,
    /// Calls the port received: `(operation, detail)`.
    calls: Vec<(&'static str, String)>,
    /// What the last credential token was.
    tokens: Vec<String>,
    /// The next pull answers window_full.
    window_full: bool,
    /// Frames the next stream yields, in order, then `Idle` forever (or `Closed` when `end`).
    frames: VecDeque<FeedItem>,
    /// The stream is closed once the frames are used up.
    end: bool,
    /// Heartbeat and idle limits.
    limits: Option<StreamLimits>,
}

struct Fake {
    script: Mutex<Script>,
}

impl Fake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(&self, ctx: &RequestCtx, op: &'static str, detail: String) -> EventsResult<()> {
        let mut script = self.lock();
        script.tokens.push(
            ctx.credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default(),
        );
        script.calls.push((op, detail));
        match script.fail.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct FakeFeed {
    frames: VecDeque<FeedItem>,
    end: bool,
    lease: String,
}

impl EventFeed for FakeFeed {
    fn lease(&self) -> &str {
        &self.lease
    }

    fn next(&mut self) -> BoxFuture<'_, FeedItem> {
        Box::pin(async move {
            if let Some(item) = self.frames.pop_front() {
                return item;
            }
            if self.end {
                return FeedItem::Closed;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            FeedItem::Idle
        })
    }
}

impl EventStreamPort for Fake {
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, EventsResult<Vec<ConsumerOut>>> {
        Box::pin(async move {
            self.record(ctx, "list", String::new())?;
            Ok(vec![consumer("billing"), consumer("stock")])
        })
    }

    fn put<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        spec: ConsumerSpecIn,
    ) -> BoxFuture<'a, EventsResult<(ConsumerOut, bool)>> {
        Box::pin(async move {
            self.record(ctx, "put", format!("{name} {spec:?}"))?;
            let created = self.lock().created;
            Ok((consumer(name), created))
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, EventsResult<ConsumerOut>> {
        Box::pin(async move {
            self.record(ctx, "get", name.to_owned())?;
            Ok(consumer(name))
        })
    }

    fn delete<'a>(&'a self, ctx: &'a RequestCtx, name: &'a str) -> BoxFuture<'a, EventsResult<()>> {
        Box::pin(async move { self.record(ctx, "delete", name.to_owned()) })
    }

    fn pull<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        request: PullIn,
    ) -> BoxFuture<'a, EventsResult<PullOut>> {
        Box::pin(async move {
            self.record(ctx, "pull", format!("{name} {lease:?} {request:?}"))?;
            let window_full = self.lock().window_full;
            Ok(PullOut {
                lease: lease.unwrap_or("lease-1").to_owned(),
                events: if window_full {
                    Vec::new()
                } else {
                    vec![
                        json!({"event_id": "0192f3c4-0000-7000-8000-00000000e001", "topic": "doc_posted"}),
                    ]
                },
                next_position: POSITION.to_owned(),
                skipped: 3,
                window_full,
            })
        })
    }

    fn ack<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        position: &'a str,
    ) -> BoxFuture<'a, EventsResult<CheckpointOut>> {
        Box::pin(async move {
            self.record(
                ctx,
                "ack",
                format!("{name} {lease:?} {partition} {position}"),
            )?;
            Ok(CheckpointOut {
                partition,
                position: position.to_owned(),
            })
        })
    }

    fn restart<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        from: StartIn,
    ) -> BoxFuture<'a, EventsResult<CheckpointOut>> {
        Box::pin(async move {
            self.record(
                ctx,
                "restart",
                format!("{name} {lease:?} {partition} {from:?}"),
            )?;
            Ok(CheckpointOut {
                partition,
                position: POSITION.to_owned(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: StreamIn,
    ) -> BoxFuture<'a, EventsResult<Box<dyn EventFeed>>> {
        Box::pin(async move {
            self.record(ctx, "stream", format!("{request:?}"))?;
            let mut script = self.lock();
            let feed = FakeFeed {
                frames: std::mem::take(&mut script.frames),
                end: script.end,
                lease: "lease-s".to_owned(),
            };
            Ok(Box::new(feed) as Box<dyn EventFeed>)
        })
    }

    fn limits(&self) -> StreamLimits {
        self.lock().limits.unwrap_or(StreamLimits {
            heartbeat: Duration::from_secs(25),
            max_idle: Duration::from_secs(900),
        })
    }
}

struct Fixture {
    router: axum::Router,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let fake = Arc::new(Fake {
        script: Mutex::new(Script::default()),
    });
    let mut ports = Ports::new();
    ports.provide::<dyn EventStreamPort>(fake.clone());
    let keys = events::required_ports();
    let state = RouteState::new(ports.subset(keys).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(events::routes().with_state(state))
    .build();
    Fixture { router, fake }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    body: Json,
}

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Json>,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    let request = match body {
        Some(json) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    f.router.clone().oneshot(request).await.unwrap()
}

async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let response = send(f, method, path, Some(TOKEN), extra, body).await;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Json::Null),
    }
}

fn code(reply: &Reply) -> &str {
    reply.body["error"]["code"].as_str().unwrap_or("")
}

fn put_body() -> Json {
    json!({
        "topics": ["doc_posted", "billing.*"],
        "filter": "subject.entity == 'sales_invoice'",
        "partitions": 2,
        "payload_mode": "ids_only",
        "start": {"kind": "tail"},
    })
}

fn fail_next(f: &Fixture, error: ApiError) {
    f.fake.lock().fail = Some(error);
}

fn last_call(f: &Fixture) -> (&'static str, String) {
    f.fake.lock().calls.last().cloned().unwrap()
}
