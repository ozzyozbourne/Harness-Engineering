(ns cmh.kernel.provider
  "Model providers. A provider is a map with :complete (fn [request] response).
  request: {:system [blocks] :messages [...] :tools [...]?}
  response: {:content [blocks] :stop_reason string}"
  (:require [clojure.data.json :as json]
            [clojure.string :as str])
  (:import [java.net URI]
           [java.net.http HttpClient HttpClient$Version HttpRequest HttpRequest$BodyPublishers
            HttpResponse$BodyHandlers]
           [java.time Duration]))

(def oauth-system-block "You are Claude Code, Anthropic's official CLI for Claude.")

(defn complete [provider request] ((:complete provider) request))

;; ---- mock --------------------------------------------------------------------

(defn mock-provider
  "Replays assistant content arrays in order, one per request, and records
  every request in (:requests provider)."
  [script]
  (let [remaining (atom (vec script))
        requests (atom [])]
    {:kind :mock
     :requests requests
     :complete (fn [req]
                 (swap! requests conj req)
                 (let [[content & more] @remaining]
                   (when-not content
                     (throw (ex-info (str "mock script exhausted at request " (count @requests)) {})))
                   (reset! remaining (vec more))
                   {:content content
                    :stop_reason (if (some #(= "tool_use" (:type %)) content) "tool_use" "end_turn")}))}))

(defn mock-from-file [path]
  (mock-provider (json/read-str (slurp path) :key-fn keyword)))

;; ---- anthropic ---------------------------------------------------------------

(defn- env [k] (let [v (System/getenv k)] (when-not (str/blank? v) v)))

(defn anthropic-provider [{:keys [model]}]
  (let [api-key (env "ANTHROPIC_API_KEY")
        token (env "CLAUDE_CODE_OAUTH_TOKEN")
        base (str/replace (or (env "ANTHROPIC_BASE_URL") "https://api.anthropic.com") #"/+$" "")
        url (URI/create (str base "/v1/messages"))
        client (-> (HttpClient/newBuilder)
                   (.version HttpClient$Version/HTTP_1_1)
                   (.connectTimeout (Duration/ofSeconds 30))
                   .build)]
    (when-not (or api-key token)
      (throw (ex-info "set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN" {})))
    {:kind :anthropic
     :complete
     (fn [{:keys [system] :as req}]
       (let [system (if api-key system (into [{:type "text" :text oauth-system-block}] system))
             body (json/write-str (-> req (assoc :model model :max_tokens 32000 :system system)))
             headers (cond-> ["content-type" "application/json"
                              "anthropic-version" "2023-06-01"]
                       api-key (into ["x-api-key" api-key])
                       (not api-key) (into ["authorization" (str "Bearer " token)
                                            "anthropic-beta" "oauth-2025-04-20"]))
             http-req (-> (HttpRequest/newBuilder url)
                          (.timeout (Duration/ofSeconds 600))
                          (.headers (into-array String headers))
                          (.POST (HttpRequest$BodyPublishers/ofString body))
                          .build)]
         (loop [attempt 0]
           (let [resp (.send client http-req (HttpResponse$BodyHandlers/ofString))
                 status (.statusCode resp)
                 text (.body resp)]
             (cond
               (= 200 status) (json/read-str text :key-fn keyword)
               (and (#{429 500 502 503 529} status) (< attempt 4))
               (do (Thread/sleep (long (* 2000 (Math/pow 2 attempt)))) (recur (inc attempt)))
               :else (throw (ex-info (str "anthropic API error " status ": "
                                          (subs text 0 (min 800 (count text))))
                                     {:status status})))))))}))
