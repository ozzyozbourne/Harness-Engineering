(ns cmh.kernel.gate
  "propose_def is the only way userspace changes: validate in scratch, go
  live as a trial, soak, then keep or revert. Every keep/revert becomes a
  verdict in knowledge."
  (:require [clojure.data.json :as json]
            [clojure.string :as str]
            [cmh.kernel.evaluator :as ev]
            [cmh.kernel.knowledge :as kn]
            [cmh.kernel.state :as st]))

(def kernel-tool-names
  "Copy of tools/names: gate cannot require tools (tools requires gate), so
  the set is repeated here. A test checks the two stay equal."
  #{"bash" "read_file" "write_file" "list_defs" "show_def" "propose_def"
    "rollback_def" "remember" "recall"})

(defn- parse-json-ish
  "Models sometimes send JSON-encoded strings for structured fields."
  [x]
  (if (string? x)
    (try (json/read-str x :key-fn keyword) (catch Exception _ x))
    x))

(defn- static-check [s {:keys [name kind source tests input_schema]}]
  (let [existing (get-in s [:defs name :kind])]
    (cond
      (not (and (string? name) (re-matches #"[A-Za-z][A-Za-z0-9_-]{0,63}" name)))
      "name must match [A-Za-z][A-Za-z0-9_-]{0,63}"
      (not (#{"tool" "note" "hook"} kind)) "kind must be tool, note or hook"
      (and existing (not= existing kind)) (str name " already exists with kind " existing)
      (and (= kind "tool") (kernel-tool-names name)) (str name " is a kernel tool and cannot be redefined")
      (and (= kind "hook") (not (contains? ev/hooks name)))
      "hook name must be compose_system or select_lessons"
      (str/blank? source) "source is empty"
      (and (not= kind "note")
           (not (and (sequential? tests) (seq tests)
                     (every? #(and (map? %) (map? (:input %)) (contains? % :expect)) tests))))
      "tools and hooks need at least one test of the form {\"input\": {...}, \"expect\": \"...\"}"
      (and (= kind "tool") (not (map? input_schema))) "tools need an input_schema object")))

(defn- verdict! [k {:keys [name version description]} outcome]
  (kn/add! k "verdict" (str "def " name " v" version " (" (pr-str (str description)) ") " outcome) "gate"))

(defn revert! [k rec error input-desc]
  (let [{:keys [name version]} rec]
    (st/emit! k {:type "def_reverted" :name name :version version :error error
                 :restored (get-in @(:state k) [:defs name :kept])})
    (verdict! k rec (str "reverted: " error (when input-desc (str " on input " input-desc))))
    (st/trace k "[gate] " name " v" version " reverted: " error)))

(defn- keep! [k rec uses]
  (let [{:keys [name version]} rec]
    (st/emit! k {:type "def_kept" :name name :version version})
    (verdict! k rec (str "kept after " uses " clean uses"))
    (st/trace k "[gate] " name " v" version " kept")))

(defn propose!
  "Run the gate. Returns {:ok bool :msg string :version n}."
  [k proposal]
  (let [s @(:state k)
        {:keys [name kind]} proposal
        version (st/next-version s name)
        tests (parse-json-ish (:tests proposal))
        rec (cond-> {:name name :kind kind :version version
                     :source (str (:source proposal))
                     :description (str (:description proposal))
                     :tests (if (= kind "note") (or tests []) tests)}
              (= kind "tool") (assoc :input_schema (or (parse-json-ish (:input_schema proposal))
                                                       {:type "object"})))
        err (static-check s rec)
        ;; A proposal is journaled whenever it has a name, even when the static
        ;; check fails; only nameless proposals are rejected without journaling.
        unjournalable? (not (and (string? name) (seq name)))]
    (if (and err unjournalable?)
      {:ok false :msg (str "rejected: " err)}
      (let [rec (-> (st/emit! k (assoc rec :type "def_proposed"))
                    (dissoc :type :ts))
            err (or err
                    (when (not= kind "note")
                      (ev/validate-in-scratch rec (:validate-ms k))))]
        (if err
          (do (st/emit! k {:type "def_rejected" :name name :version version :error err})
              (st/trace k "[gate] " name " v" version " rejected: " err)
              {:ok false :version version :msg (str "rejected v" version ": " err
                                                    " (live version unchanged)")})
          (do (when-let [cur (st/live-record @(:state k) name)]
                (when (= "trial" (:status cur))
                  (revert! k cur (str "superseded by v" version) nil)))
              (st/emit! k {:type "def_trial" :name name :version version})
              {:ok true :version version
               :msg (str "trial v" version ": " name " is live now; kept after "
                         (:soak k) " clean uses, reverted on the first error")}))))))

(defn rollback!
  "Reinstall an older version's source as a new trial version."
  [k name version]
  (if-let [old (get-in @(:state k) [:defs name :versions version])]
    (propose! k (select-keys old st/def-fields))
    {:ok false :msg (str "no version " version " of " name)}))

(defn record-use!
  "Journal one use and run soak accounting for trial versions."
  [k {:keys [name version status] :as rec} ok error input-desc]
  (st/emit! k {:type "def_use" :name name :version version :ok ok :error error})
  (when (= "trial" status)
    (if-not ok
      (revert! k rec error input-desc)
      (let [uses (get-in @(:state k) [:defs name :uses] 0)]
        (when (>= uses (:soak k)) (keep! k rec uses))))))

(defn call-live
  "Late-bound call: look up the live version of name now, call it, record the
  use. Returns {:ok true :value v} or {:ok false :error e}, plus :version,
  :status, or {:missing true} when nothing is live."
  [k name input input-desc]
  (if-let [rec (st/live-record @(:state k) name)]
    (let [res (try {:ok true
                    :value (ev/invoke (:kind rec) name (ev/live-entry rec (:validate-ms k))
                                      (ev/args-for (:kind rec) name input) (:call-ms k))}
                   (catch Throwable e {:ok false :error (ev/describe-error e)}))]
      (record-use! k rec (:ok res) (:error res) input-desc)
      (assoc res :version (:version rec) :status (:status rec)))
    {:ok false :missing true :error (str "no live definition named " name)}))
