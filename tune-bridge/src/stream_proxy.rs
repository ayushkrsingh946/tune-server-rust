use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;
use tracing::warn;

use crate::state::{CorpsRelaye, PendingResponse, RelayState};

pub async fn proxy_stream(
    State(state): State<Arc<RelayState>>,
    Path((server_id, stream_path)): Path<(String, String)>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let conn = match state.servers.get(&server_id) {
        Some(c) => c,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Auth : meme regle que le proxy d'API — `X-Bridge-Token` d'abord,
    // l'ancienne forme `Authorization: BridgeToken …` ensuite. Le commentaire
    // d'origine annoncait « from query or header » alors que seul l'en-tete
    // etait lu ; le lecteur audio du navigateur, lui, ne peut PAS poser
    // d'en-tete sur la source d'une balise <audio>, d'ou le parametre de
    // requete accepte ici.
    let jeton = crate::api_proxy::extraire_jeton(&headers)
        .or_else(|| crate::api_proxy::jeton_de_requete(uri.query()));
    let auth_ok = jeton
        .as_deref()
        .map(|t| state.server_for_token(t).as_deref() == Some(&*server_id))
        .unwrap_or(false);

    if !auth_ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let stream_req = message_de_flux(&request_id, &stream_path, &headers);

    let (tx, rx) = oneshot::channel::<PendingResponse>();
    conn.pending.lock().await.insert(request_id.clone(), tx);

    if conn.ws_tx.send(stream_req.to_string()).await.is_err() {
        conn.pending.lock().await.remove(&request_id);
        return StatusCode::BAD_GATEWAY.into_response();
    }

    drop(conn);

    // Wait for the relay.stream_start response with headers/status
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(resp)) => reponse_relayee(resp),
        Ok(Err(_)) => {
            warn!(request_id = %request_id, "stream response channel dropped");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(_) => {
            if let Some(conn) = state.servers.get(&server_id) {
                conn.pending.lock().await.remove(&request_id);
            }
            warn!(request_id = %request_id, "stream request timeout");
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

/// Le message `relay.stream_request` d'une lecture : le `Range` ET le
/// `User-Agent` du lecteur.
///
/// #6453 — le serveur choisit le contrat d'une radio selon le lecteur
/// (`accepts_chunked_live_stream`, tune-stream-http) : corps chunké pour un
/// navigateur ou un renderer Lavf, « fichier borné » pour les autres, dont
/// AVPlayer. Sans l'agent, la requête locale du serveur n'en portait aucun, et
/// l'iPhone recevait le contrat chunké, qu'AVPlayer rejette
/// (`CoreMediaErrorDomain -12939`). Un serveur plus ancien ignore le champ.
pub fn message_de_flux(
    request_id: &str,
    stream_path: &str,
    headers: &HeaderMap,
) -> serde_json::Value {
    let entete = |nom: &str| {
        headers
            .get(nom)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    serde_json::json!({
        "type": "relay.stream_request",
        "id": request_id,
        "stream_id": stream_path,
        "range": entete("range"),
        "user_agent": entete("user-agent"),
    })
}

/// Batit la reponse HTTP a partir de ce que le serveur a renvoye.
///
/// Un flux audio ne peut pas etre mis en memoire avant d'etre rendu : le
/// corps est cable directement sur le canal de morceaux, pour que le lecteur
/// entende le debut du morceau pendant que la fin voyage encore.
pub fn reponse_relayee(resp: PendingResponse) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);

    for (key, value) in &resp.headers {
        if let Some(v) = value.as_str() {
            response = response.header(key.as_str(), v);
        }
    }

    let corps = match resp.body {
        CorpsRelaye::Entier(body) => Body::from(body.unwrap_or_default()),
        CorpsRelaye::Morceaux(reception) => {
            let morceaux = futures_util::stream::unfold(reception, |mut rx| async move {
                rx.recv()
                    .await
                    .map(|morceau| (Ok::<Vec<u8>, std::io::Error>(morceau), rx))
            });
            Body::from_stream(morceaux)
        }
    };

    response
        .body(corps)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::message_de_flux;
    use axum::http::HeaderMap;

    /// #6453 — l'agent d'AVPlayer voyage jusqu'au serveur avec le `Range`.
    #[test]
    fn le_message_de_flux_porte_le_range_et_l_agent_du_lecteur() {
        let mut h = HeaderMap::new();
        h.insert("range", "bytes=0-1".parse().unwrap());
        h.insert(
            "user-agent",
            "AppleCoreMedia/1.0.0.24A446 (iPhone; U; CPU OS 27_0_1 like Mac OS X; fr_fr)"
                .parse()
                .unwrap(),
        );
        let m = message_de_flux("req-1", "abc.flac", &h);
        assert_eq!(m["type"], "relay.stream_request");
        assert_eq!(m["id"], "req-1");
        assert_eq!(m["stream_id"], "abc.flac");
        assert_eq!(m["range"], "bytes=0-1");
        assert!(
            m["user_agent"]
                .as_str()
                .is_some_and(|ua| ua.starts_with("AppleCoreMedia/")),
            "l'agent du lecteur doit partir vers le serveur : {m}"
        );
    }

    /// Sans en-têtes, les deux champs valent `null` : le serveur garde son
    /// comportement d'avant (agent absent → contrat chunké).
    #[test]
    fn sans_entetes_les_champs_sont_nuls() {
        let m = message_de_flux("req-2", "abc", &HeaderMap::new());
        assert!(m["range"].is_null());
        assert!(m["user_agent"].is_null());
    }
}
