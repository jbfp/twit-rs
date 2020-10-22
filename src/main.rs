#[macro_use]
extern crate actix_web;

#[macro_use]
extern crate serde_json;

use actix_web::{
    cookie::Cookie, cookie::SameSite, dev::HttpResponseBuilder,
    http::header::IntoHeaderValue, web::Bytes, web::Data, web::Form, App, Error, HttpMessage,
    HttpRequest, HttpResponse, HttpServer,
};
use chrono::prelude::*;
use handlebars::Handlebars;
use listenfd::ListenFd;
use serde::{Deserialize, Serialize};
use std::{
    io::Result as IoResult,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    stream::Stream,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Mutex, RwLock,
    },
};

#[actix_web::main]
async fn main() -> IoResult<()> {
    let mut listenfd = ListenFd::from_env();
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars
        .register_templates_directory(".hbs", "./static/templates")
        .unwrap();

    let handlebars_ref = Data::new(handlebars);

    let tweets_ref = Data::new(Tweets::new());

    let broadcaster_ref = Data::new(Broadcaster::new());

    let mut server = HttpServer::new(move || {
        App::new()
            .app_data(broadcaster_ref.clone())
            .app_data(handlebars_ref.clone())
            .app_data(tweets_ref.clone())
            .service(index)
            .service(login)
            .service(logout)
            .service(get_tweets)
            .service(new_tweet)
            .service(create_tweet)
            .service(stream_tweets)
    });

    server = if let Some(l) = listenfd.take_tcp_listener(0).unwrap() {
        server.listen(l)?
    } else {
        server.bind("127.0.0.1:5000")?
    };

    server.run().await
}

#[get("/")]
async fn index(hb: Data<Handlebars<'_>>, req: HttpRequest) -> HttpResponse {
    let json = json!({ "username": get_username(&req) });
    let body = hb.render("index", &json).unwrap();
    HttpResponse::Ok().body(body)
}

fn get_username(req: &HttpRequest) -> Option<String> {
    req.cookie("username").map(|c| c.value().to_owned())
}

#[derive(Debug, Deserialize)]
struct Login {
    username: String,
}

#[post("/login")]
async fn login(form: Form<Login>) -> HttpResponse {
    let mut builder = redirect("/");

    if !form.username.trim().is_empty() {
        let cookie = Cookie::build("username", form.username.clone())
            .http_only(true)
            .same_site(SameSite::Strict)
            .secure(true)
            .finish();

        builder.cookie(cookie);
    }

    builder.finish()
}

#[post("/logout")]
async fn logout(req: HttpRequest) -> HttpResponse {
    let mut builder = redirect("/");

    if let Some(ref cookie) = req.cookie("username") {
        builder.del_cookie(cookie);
    }

    builder.finish()
}

#[get("/tweets")]
async fn get_tweets(hb: Data<Handlebars<'_>>, tweets: Data<Tweets>) -> HttpResponse {
    let tweets = tweets.cloned().await;
    let data = json!({ "tweets": tweets });
    let body = hb.render("tweets", &data).unwrap();
    HttpResponse::Ok().body(body)
}

#[get("/tweet")]
async fn new_tweet(hb: Data<Handlebars<'_>>, req: HttpRequest) -> HttpResponse {
    if get_username(&req).is_none() {
        redirect("/").finish()
    } else {
        let data = json!({});
        let body = hb.render("new-tweet", &data).unwrap();
        HttpResponse::Ok().body(body)
    }
}

#[derive(Debug, Deserialize)]
struct NewTweet {
    content: String,
}

#[post("/tweet")]
async fn create_tweet(
    hb: Data<Handlebars<'_>>,
    form: Form<NewTweet>,
    tweets: Data<Tweets>,
    req: HttpRequest,
    broadcaster: Data<Broadcaster>,
) -> HttpResponse {
    match get_username(&req) {
        None => redirect("/").finish(),
        Some(username) => {
            if form.content.trim().is_empty() {
                let data = json!({
                    "error": "content is empty"
                });

                let body = hb.render("new-tweet", &data).unwrap();

                HttpResponse::Ok().body(body)
            } else {
                tweets.insert(Tweet {
                    author: username,
                    content: form.content.clone(),
                    timestamp: Utc::now(),
                }).await;

                broadcaster.send("").await;

                redirect("/").finish()
            }
        }
    }
}

#[get("/stream")]
async fn stream_tweets(broadcaster: Data<Broadcaster>) -> HttpResponse {
    let rx = broadcaster.subscribe().await;

    HttpResponse::Ok()
        .content_type("text/event-stream")
        .set_header("Cache-Control", "no-transform")
        .keep_alive()
        .streaming(rx)
}

fn redirect<I: IntoHeaderValue>(value: I) -> HttpResponseBuilder {
    let mut builder = HttpResponse::SeeOther();
    builder.set_header("Location", value);
    builder
}

#[derive(Clone, Debug, Serialize)]
struct Tweet {
    author: String,
    content: String,
    timestamp: DateTime<Utc>,
}

#[derive(Clone)]
struct Tweets(Arc<RwLock<Vec<Tweet>>>);

impl Tweets {
    fn new() -> Self {
        Self(Arc::new(RwLock::new(Vec::new())))
    }
    
    async fn insert(&self, tweet: Tweet) {
        self.0.write().await.insert(0, tweet);
    }

    async fn cloned(&self) -> Vec<Tweet> {
        self.0.read().await.clone()
    }
}

#[derive(Clone)]
struct Broadcaster(Arc<Mutex<Vec<Sender<Bytes>>>>);

impl Broadcaster {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    async fn subscribe(&self) -> Client {
        let (tx, rx) = channel(100);
        self.0.lock().await.push(tx);
        Client::new(rx)
    }

    async fn send(&self, msg: &str) {
        let msg = Bytes::from(["data: ", msg, "\n\n"].concat());
        let guard = self.0.lock().await;
        let clients = guard.as_slice();

        for client in clients {
            let _ = client.send(msg.clone()).await;
        }
    }
}

struct Client(Receiver<Bytes>);

impl Client {
    fn new(rx: Receiver<Bytes>) -> Self {
        Client(rx)
    }
}

impl Stream for Client {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.0).poll_next(cx) {
            Poll::Ready(Some(v)) => Poll::Ready(Some(Ok(v))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
