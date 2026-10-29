(ns cmh.kernel.agent
  "Kernel assembly, the agent loop, reflection, scoring and refine."
  (:require [clojure.data.json :as json]
            [clojure.edn :as edn]
            [clojure.java.io :as io]
            [clojure.string :as str]
            [cmh.kernel.base-prompt :as bp]
            [cmh.kernel.evaluator :as ev]
            [cmh.kernel.gate :as gate]
            [cmh.kernel.knowledge :as kn]
            [cmh.kernel.provider :as provider]
            [cmh.kernel.state :as st]
            [cmh.kernel.tools :as tools]))

(defn home-dir
  "The flavor directory (clojure/)."
  []
  (or (System/getProperty "cmh.home")
      (when-let [u (io/resource "cmh/kernel/agent.clj")]
        (when (= "file" (.getProtocol u))
          (-> (io/file (.toURI u)) .getParentFile .getParentFile .getParentFile .getParentFile
              .getCanonicalPath)))
      (System/getProperty "user.dir")))

(defn- install-seeds! [k]
  (let [dir (io/file (:home k) "seed")]
    (doseq [{:keys [file] :as seed} (edn/read-string (slurp (io/file dir "seeds.edn")))]
      (st/emit! k (-> seed (dissoc :file)
                      (assoc :type "def_proposed" :version 1 :source (slurp (io/file dir file)))))
      (st/emit! k {:type "def_kept" :name (:name seed) :version 1}))))

(defn open
  "Open a kernel on a state dir: replay the journal, seed if empty."
  [{:keys [state-dir provider soak reflect trace home]
    :or {state-dir ".cmh" soak 3 reflect true}}]
  (let [home (or home (home-dir))
        sd (.getCanonicalFile (io/file state-dir))
        _ (.mkdirs sd)
        journal (io/file sd "journal.jsonl")
        events (st/read-journal journal)
        k {:home home
           :state-dir sd
           :journal journal
           :kernel-dir (io/file home "src" "cmh" "kernel")
           :state (atom (st/replay events))
           :provider provider
           :soak soak
           :reflect reflect
           :validate-ms 10000
           :call-ms 30000
           :max-turns 40
           :base-prompt (bp/load-base-prompt home)
           :trace (or trace (fn [line] (binding [*out* *err*] (println line) (flush))))}]
    (when (empty? events) (install-seeds! k))
    k))

(defn- trace [k & xs] ((:trace k) (apply str xs)))

(defn- clip [s n] (let [s (str s)] (if (> (count s) n) (str (subs s 0 n) "...") s)))

;; ---- prompt assembly ------------------------------------------------------------

(defn- call-hook
  "Call a hook late-bound. A failing trial is reverted by the gate and the
  restored version is tried once; otherwise fall back to the builtin and
  record a kernel error."
  [k errors name input desc fallback]
  (loop [tries 0]
    (let [r (gate/call-live k name input desc)]
      (cond
        (:ok r) (:value r)
        (:missing r) (fallback)
        (and (= "trial" (:status r)) (zero? tries)) (recur 1)
        :else (do (swap! errors conj (str name " v" (:version r) ": " (:error r)))
                  (trace k "[kernel] hook " name " failed: " (:error r) "; using builtin")
                  (fallback))))))

(defn select-lessons [k errors task]
  (let [lessons (mapv #(select-keys % [:id :text :score]) (st/active-lessons @(:state k)))
        valid (set (map :id lessons))]
    (if (empty? lessons)
      []
      (->> (call-hook k errors "select_lessons" {:task task :lessons lessons}
                      (str "(task " (pr-str (clip task 80)) ", " (count lessons) " lessons)")
                      #(mapv :id (take 3 (sort-by (comp - :score) lessons))))
           (map str) (filter valid) distinct vec))))

(defn supplemental [k errors lesson-ids]
  (let [s @(:state k)
        notes (mapv :source (st/live-defs s "note"))
        lessons (vec (keep #(some-> (get-in s [:knowledge %]) :text) lesson-ids))
        composed (call-hook k errors "compose_system" {:notes notes :lessons lessons}
                            (str "(" (count notes) " notes, " (count lessons) " lessons)")
                            #(str/join "\n\n" (concat notes lessons)))
        verdicts (take-last 20 (st/verdicts s))]
    (str/trim
     (str composed
          (when (seq verdicts)
            (str "\n\n## Verdicts on past userspace changes\n"
                 (str/join "\n" (map #(str "- " (:text %)) verdicts))))))))

(defn system-blocks [k errors lesson-ids]
  (let [sup (supplemental k errors lesson-ids)]
    (cond-> [{:type "text" :text (:base-prompt k)}]
      (not (str/blank? sup)) (conj {:type "text" :text sup}))))

(defn tool-list [k]
  (into (vec tools/specs)
        (for [r (st/live-defs @(:state k) "tool")]
          {:name (:name r) :description (or (:description r) "") :input_schema (:input_schema r)})))

;; ---- tool dispatch ----------------------------------------------------------------

(defn exec-tool [k name input]
  (if (tools/names name)
    (tools/run-tool k name input)
    (let [rec (st/live-record @(:state k) name)]
      (if-not (= "tool" (:kind rec))
        {:content (str "unknown tool " name) :is_error true}
        (let [r (gate/call-live k name input (clip (json/write-str input) 200))]
          (if (:ok r)
            {:content (:value r)}
            {:content (str "error in " name " v" (:version r) ": " (:error r)
                           (when (= "trial" (:status r)) " (trial version reverted)"))
             :is_error true}))))))

(defn- trace-tool [k name input {:keys [content is_error verdict]}]
  (trace k "[tool] " name
         (cond
           (#{"propose_def" "rollback_def"} name)
           (str " " (:name input) " -> " (first (str/split-lines (str (:msg verdict)))))
           :else
           (str (when-let [v (first (vals input))] (str " " (pr-str (clip v 60))))
                " -> " (when is_error "ERROR ")
                (clip (str/replace (str content) #"\s+" " ") 100)))))

;; ---- the loop -----------------------------------------------------------------------

(defn- text-of [content]
  (str/trim (str/join "\n" (keep #(when (= "text" (:type %)) (:text %)) content))))

(defn reflect! [k transcript]
  (try
    (let [prompt (str "A task just finished. Compact transcript:\n\n" transcript
                      "\n\nReply with only a JSON array of 0 to 3 short, reusable lessons (strings) "
                      "that would help with future tasks. Skip task-specific trivia. [] is fine.")
          resp (provider/complete (:provider k)
                                  {:system [{:type "text" :text (:base-prompt k)}]
                                   :messages [{:role "user" :content prompt}]})
          text (text-of (:content resp))
          arr (re-find #"(?s)\[.*\]" text)
          items (when arr (json/read-str arr))
          added (->> (if (sequential? items) items [])
                     (filter string?) (map str/trim) (remove str/blank?)
                     (filter #(<= (count %) 400)) (take 3)
                     (keep #(kn/add! k "lesson" % "reflection"))
                     doall)]
      (trace k "[reflect] " (count added) " new lesson(s)"))
    (catch Throwable e
      (trace k "[reflect] failed: " (ev/describe-error e)))))

(defn run-task!
  "Run one task. on-text is called with each assistant text. Returns
  {:success bool :answer string :stop_reason s :errors [...]}."
  ([k task] (run-task! k task {}))
  ([k task {:keys [on-text] :or {on-text (fn [_])}}]
   (let [id (str (random-uuid))
         errors (atom [])
         lesson-ids (select-lessons k errors task)
         trial-notes (filter #(= "trial" (:status %)) (st/live-defs @(:state k) "note"))
         _ (st/emit! k {:type "task_started" :id id :task task :lessons lesson-ids})
         transcript (atom [(str "TASK: " (clip task 500))])
         result
         (loop [messages [{:role "user" :content task}] tool-errors [] turn 0 answer ""]
           (if (>= turn (:max-turns k))
             (do (swap! errors conj "max turns exceeded")
                 {:stop_reason "max_turns" :answer answer :tool-errors tool-errors})
             (let [resp (try (provider/complete (:provider k)
                                                {:system (system-blocks k errors lesson-ids)
                                                 :messages messages
                                                 :tools (tool-list k)})
                             (catch Throwable e
                               (swap! errors conj (str "model request failed: " (ev/describe-error e)))
                               (trace k "[kernel] model request failed: " (ev/describe-error e))
                               nil))]
               (if-not resp
                 {:stop_reason "error" :answer answer :tool-errors tool-errors}
                 (let [content (vec (:content resp))
                       text (text-of content)
                       answer (if (str/blank? text) answer text)
                       messages (conj messages {:role "assistant" :content content})
                       uses (filter #(= "tool_use" (:type %)) content)]
                   (when-not (str/blank? text)
                     (on-text text)
                     (swap! transcript conj (str "ASSISTANT: " (clip text 400))))
                   (if (and (= "tool_use" (:stop_reason resp)) (seq uses))
                     (let [results (vec (for [{:keys [id name input]} uses]
                                          (let [input (or input {})
                                                r (exec-tool k name input)]
                                            (trace-tool k name input r)
                                            (swap! transcript conj
                                                   (str "TOOL " name " " (clip (json/write-str input) 200)
                                                        " -> " (when (:is_error r) "ERROR ")
                                                        (clip (:content r) 200)))
                                            (assoc r :id id))))
                           blocks (mapv (fn [{:keys [id content is_error]}]
                                          (cond-> {:type "tool_result" :tool_use_id id
                                                   :content (if (str/blank? content) "(empty)" content)}
                                            is_error (assoc :is_error true)))
                                        results)]
                       (recur (conj messages {:role "user" :content blocks})
                              (into tool-errors (map #(boolean (:is_error %)) results))
                              (inc turn) answer))
                     {:stop_reason (:stop_reason resp) :answer answer :tool-errors tool-errors}))))))
         success (and (= "end_turn" (:stop_reason result))
                      (empty? @errors)
                      (not-any? true? (take-last 2 (:tool-errors result))))
         error (cond (seq @errors) (str/join "; " @errors)
                     (not= "end_turn" (:stop_reason result)) (str "stop_reason " (:stop_reason result))
                     (not success) "tool error in final tool results")]
     (st/emit! k {:type "task_finished" :id id :success success :error error})
     (trace k "[task] " (if success "success" (str "failed: " error)))
     (kn/score! k lesson-ids success)
     ;; notes soak over tasks
     (doseq [n trial-notes]
       (when (= (:version n) (get-in @(:state k) [:defs (:name n) :live]))
         (gate/record-use! k n success (when-not success (str "task failed: " error)) nil)))
     (when (:reflect k)
       (swap! transcript conj (str "OUTCOME: " (if success "success" (str "failed: " error))))
       (reflect! k (str/join "\n" @transcript)))
     (assoc result :success success :error error :errors @errors))))

(defn refine-prompt [k]
  (let [s @(:state k)
        tasks (take-last 10 (:tasks s))
        verdicts (take-last 15 (st/verdicts s))]
    (str "Refine yourself. Review the recent task outcomes, verdicts, and current userspace below, "
         "then improve userspace through propose_def or knowledge through remember. Make at most 3 "
         "changes and prefer small ones. Use show_def before changing a definition. If nothing clearly "
         "needs improving, make no changes and say so.\n\n"
         "## Recent tasks\n"
         (if (seq tasks)
           (str/join "\n" (map #(str "- " (case (:success %) true "success" false "FAILED" "unfinished")
                                     ": " (clip (:task %) 200)
                                     (when (:error %) (str " (" (clip (:error %) 200) ")")))
                               tasks))
           "(none)")
         "\n\n## Verdicts\n"
         (if (seq verdicts) (str/join "\n" (map #(str "- " (:text %)) verdicts)) "(none)")
         "\n\n## Userspace\n" (tools/defs-table s)
         "\n\n## Lessons\n"
         (let [ls (st/active-lessons s)]
           (if (seq ls) (str/join "\n" (map #(str "- " (:id %) " [" (:score %) "] " (:text %)) ls)) "(none)")))))

(defn refine! [k opts] (run-task! k (refine-prompt k) opts))
