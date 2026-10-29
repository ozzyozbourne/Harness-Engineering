(ns cmh.kernel.tools
  "Kernel tools: always present, not redefinable."
  (:require [clojure.data.json :as json]
            [clojure.java.io :as io]
            [clojure.string :as str]
            [cmh.kernel.evaluator :as ev]
            [cmh.kernel.gate :as gate]
            [cmh.kernel.knowledge :as kn]
            [cmh.kernel.state :as st])
  (:import [java.io File]
           [java.util.concurrent TimeUnit]))

(defn- obj [props & required]
  {:type "object" :properties props :required (vec required)})

(def specs
  [{:name "bash"
    :description "Run a bash command in the working directory. Returns combined stdout/stderr and the exit code (truncated to 20 KB)."
    :input_schema (obj {:command {:type "string"}
                        :timeout_s {:type "integer" :description "default 60"}} "command")}
   {:name "read_file" :description "Read a file (truncated to 50 KB)."
    :input_schema (obj {:path {:type "string"}} "path")}
   {:name "write_file" :description "Write a file. Refuses the state dir and the harness kernel source."
    :input_schema (obj {:path {:type "string"} :content {:type "string"}} "path" "content")}
   {:name "list_defs" :description "List userspace definitions: name, kind, live version, status, description."
    :input_schema (obj {})}
   {:name "show_def" :description "Show a definition version (default: live) with source and tests."
    :input_schema (obj {:name {:type "string"} :version {:type "integer"}} "name")}
   {:name "propose_def"
    :description (str "Propose a new version of a userspace definition. It is validated in a scratch namespace, "
                      "goes live as a trial, and is kept after clean uses or reverted on error. "
                      "Language: Clojure. kind=tool: source must define (defn run [input] ...) where input is a "
                      "keyword-keyed map and the result is a string; give input_schema. kind=hook: name "
                      "compose_system defines (defn compose-system [notes lessons] ...) -> string (both are vectors "
                      "of strings); name select_lessons defines (defn select-lessons [task lessons] ...) -> vector of "
                      "ids (lessons are maps with :id :text :score). kind=note: source is plain prompt text. "
                      "tests: [{\"input\": {...}, \"expect\": \"substring of output\"}], at least one for tools and "
                      "hooks; hook test input is {\"notes\",\"lessons\"} or {\"task\",\"lessons\"}; select_lessons "
                      "output is compared JSON-encoded. Use clojure.string etc. via (require ...).")
    :input_schema (obj {:name {:type "string"}
                        :kind {:type "string" :enum ["tool" "note" "hook"]}
                        :source {:type "string"}
                        :description {:type "string"}
                        :tests {:type "array"
                                :items {:type "object"
                                        :properties {:input {:type "object"} :expect {:type "string"}}
                                        :required ["input" "expect"]}}
                        :input_schema {:type "object" :description "JSON Schema of the tool input (tools only)"}}
                       "name" "kind" "source" "description")}
   {:name "rollback_def" :description "Reinstall an older version as a new trial version."
    :input_schema (obj {:name {:type "string"} :version {:type "integer"}} "name" "version")}
   {:name "remember" :description "Add a durable, reusable lesson to knowledge."
    :input_schema (obj {:text {:type "string"}} "text")}
   {:name "recall" :description "Search lessons and verdicts that share words with the query."
    :input_schema (obj {:query {:type "string"}} "query")}])

(def names (set (map :name specs)))

(defn- truncate [s n]
  (if (> (count s) n) (str (subs s 0 n) "\n[truncated " (- (count s) n) " chars]") s))

(defn- bash [{:keys [command timeout_s]}]
  (let [t (long (or timeout_s 60))
        p (-> (ProcessBuilder. ["bash" "-c" (str command)])
              (.directory (io/file (System/getProperty "user.dir")))
              (.redirectErrorStream true)
              .start)
        out (future (slurp (.getInputStream p)))]
    (.close (.getOutputStream p))
    (if (.waitFor p t TimeUnit/SECONDS)
      {:content (truncate (str @out "\n[exit code " (.exitValue p) "]") 20000)}
      (do (.destroyForcibly p)
          {:content (truncate (str (deref out 1000 "") "\n[timed out after " t " s]") 20000)
           :is_error true}))))

(defn- canonical ^String [p] (.getCanonicalPath (io/file p)))

(defn- inside? [path dir]
  (let [p (canonical path) d (canonical dir)]
    (or (= p d) (str/starts-with? p (str d File/separator)))))

(defn- write-file [k {:keys [path content]}]
  (cond
    (str/blank? path) {:content "path is required" :is_error true}
    (inside? path (:state-dir k)) {:content "refused: path is inside the state dir" :is_error true}
    (inside? path (:kernel-dir k)) {:content "refused: path is inside the kernel source" :is_error true}
    :else (let [f (io/file path)]
            (when-let [parent (.getParentFile (.getAbsoluteFile f))] (.mkdirs parent))
            (spit f (str content))
            {:content (str "wrote " (count (str content)) " chars to " path)})))

(defn defs-table [s]
  (let [rows (for [name (sort (keys (:defs s)))
                   :let [d (get-in s [:defs name])
                         latest (get-in d [:versions (apply max (keys (:versions d)))])
                         live (st/live-record s name)
                         r (or live latest)]]
               (str name "  " (:kind d) "  "
                    (if live (str "v" (:version live)) "none")
                    "  " (if live (:status live) (str "no live version (latest v" (:version latest) " " (:status latest) ")"))
                    (when (= "trial" (:status live)) (str " (" (:uses d 0) " clean uses)"))
                    "  " (:description r)))]
    (if (seq rows) (str/join "\n" rows) "(no definitions)")))

(defn- show-def [k {:keys [name version]}]
  (let [s @(:state k)
        v (or (some-> version str parse-long) (get-in s [:defs name :live])
              (some->> (keys (get-in s [:defs name :versions])) seq (apply max)))]
    (if-let [rec (get-in s [:defs name :versions v])]
      {:content (json/write-str rec :indent true)}
      {:content (str "no such definition/version: " name " " version) :is_error true})))

(defn- knowledge-line [e]
  (str (:id e) " [" (if (= "verdict" (:kind e)) "verdict" (str "score " (:score e))) "] " (:text e)))

(defn run-tool
  "Execute a kernel tool. Returns {:content string :is_error bool}."
  [k name input]
  (try
    (case name
      "bash" (bash input)
      "read_file" (let [f (io/file (str (:path input)))]
                    (if (.isFile f)
                      {:content (truncate (slurp f) 50000)}
                      {:content (str "no such file: " (:path input)) :is_error true}))
      "write_file" (write-file k input)
      "list_defs" {:content (defs-table @(:state k))}
      "show_def" (show-def k input)
      "propose_def" (let [r (gate/propose! k input)]
                      {:content (:msg r) :is_error (not (:ok r)) :verdict r})
      "rollback_def" (let [r (gate/rollback! k (str (:name input)) (some-> (:version input) str parse-long))]
                       {:content (:msg r) :is_error (not (:ok r)) :verdict r})
      "remember" (if-let [id (kn/add! k "lesson" (:text input) "model")]
                   {:content (str "remembered as " id)}
                   {:content "not added: empty or duplicate of an existing lesson"})
      "recall" (let [hits (kn/recall @(:state k) (:query input))]
                 {:content (if (seq hits) (str/join "\n" (map knowledge-line hits)) "(nothing found)")}))
    (catch Throwable e
      {:content (str name " failed: " (ev/describe-error e)) :is_error true})))
