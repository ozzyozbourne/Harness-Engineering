(ns cmh.kernel-test
  (:require [clojure.data.json :as json]
            [clojure.java.io :as io]
            [clojure.string :as str]
            [clojure.test :refer [deftest is testing use-fixtures]]
            [cmh.kernel.agent :as agent]
            [cmh.kernel.evaluator :as ev]
            [cmh.kernel.gate :as gate]
            [cmh.kernel.knowledge :as kn]
            [cmh.kernel.main :as main]
            [cmh.kernel.provider :as provider]
            [cmh.kernel.state :as st]
            [cmh.kernel.tools :as tools])
  (:import [java.nio.file Files]
           [java.nio.file.attribute FileAttribute]))

(def ^:dynamic *dir* nil)

(defn- delete-tree [f]
  (when (.isDirectory f) (run! delete-tree (.listFiles f)))
  (.delete f))

(use-fixtures :each
  (fn [t]
    (let [d (.toFile (Files/createTempDirectory "cmh-test" (make-array FileAttribute 0)))]
      (try (binding [*dir* (str d)] (t)) (finally (delete-tree d))))))

(defn- open-k [& {:keys [script soak] :or {script [] soak 3}}]
  (agent/open {:state-dir *dir* :provider (provider/mock-provider script)
               :soak soak :reflect false :trace (fn [_])}))

(defn- with-script [k script] (assoc k :provider (provider/mock-provider script)))

(defn- live [k name] (st/live-record @(:state k) name))
(defn- status [k name v] (get-in @(:state k) [:defs name :versions v :status]))
(defn- call [k name input] (gate/call-live k name input (json/write-str input)))

(def wc-v2
  {:name "word_count" :kind "tool" :description "prefixed count"
   :source "(defn run [input] (str \"words=\" (count (re-seq #\"\\S+\" (:text input)))))"
   :tests [{:input {:text "a b"} :expect "words=2"}]
   :input_schema {:type "object" :properties {:text {:type "string"}}}})

(def wc-throwing
  {:name "word_count" :kind "tool" :description "ratio-based count"
   :source "(defn run [input]
              (let [t (:text input) n (count (re-seq #\"\\S+\" t))]
                (str (* n (quot (count t) (count t))))))"
   :tests [{:input {:text "a b"} :expect "2"}]
   :input_schema {:type "object"}})

(defn- text [s] {:type "text" :text s})
(defn- tool-use [id name input] {:type "tool_use" :id id :name name :input input})
(defn- system-text [req] (str/join "\n" (map :text (:system req))))

(deftest seeds-pass-their-own-tests
  (let [k (open-k)]
    (doseq [n ["word_count" "compose_system" "select_lessons"]]
      (is (= "kept" (:status (live k n))))
      (is (nil? (ev/validate-in-scratch (live k n) 5000)) n))))

(deftest late-binding
  (let [k (open-k)]
    (is (= "3" (:value (call k "word_count" {:text "x y z"}))))
    (let [r (gate/propose! k wc-v2)]
      (is (:ok r))
      (is (= 2 (:version (live k "word_count")))))
    (testing "the very next call in the same process uses v2"
      (is (= "words=3" (:value (call k "word_count" {:text "x y z"})))))
    (testing "scratch namespaces are cleaned up"
      (is (empty? (filter #(str/starts-with? (str (ns-name %)) "cmh.scratch.") (all-ns)))))))

(deftest gate-rejection
  (let [k (open-k)]
    (is (= tools/names gate/kernel-tool-names) "gate's copy of the kernel tool names is current")
    (testing "source that fails to eval"
      (let [r (gate/propose! k (assoc wc-v2 :source "(defn run [input] (str (:text input)"))]
        (is (not (:ok r)))
        (is (re-find #"eval failed" (:msg r)))
        (is (= "rejected" (status k "word_count" 2)))))
    (testing "source that fails a test"
      (let [r (gate/propose! k (assoc wc-v2 :tests [{:input {:text "a b"} :expect "words=99"}]))]
        (is (not (:ok r)))
        (is (re-find #"test 1 failed" (:msg r)))
        (is (= "rejected" (status k "word_count" 3)))))
    (testing "missing entry point and missing tests"
      (is (not (:ok (gate/propose! k (assoc wc-v2 :source "(defn go [x] \"1\")")))))
      (is (not (:ok (gate/propose! k (assoc wc-v2 :tests []))))))
    (is (= 1 (:version (live k "word_count"))))
    (is (= "kept" (:status (live k "word_count"))))
    (is (= "2" (:value (call k "word_count" {:text "a b"}))))))

(deftest soak-revert
  (let [k (open-k)]
    (is (:ok (gate/propose! k wc-throwing)))
    (is (= "trial" (:status (live k "word_count"))))
    (is (= "2" (:value (call k "word_count" {:text "a b"}))))
    (let [r (call k "word_count" {:text ""})]
      (is (not (:ok r)))
      (is (re-find #"ArithmeticException" (:error r))))
    (is (= "reverted" (status k "word_count" 2)))
    (is (= 1 (:version (live k "word_count"))))
    (is (= "0" (:value (call k "word_count" {:text ""}))))
    (let [v (last (st/verdicts @(:state k)))]
      (is (re-find #"def word_count v2 .*reverted: ArithmeticException.* on input \{\"text\":\"\"\}" (:text v))))))

(deftest soak-revert-removes-new-definition
  (let [k (open-k)]
    (is (:ok (gate/propose! k {:name "boom" :kind "tool" :description "d" :input_schema {:type "object"}
                               :source "(defn run [input] (if (:x input) (throw (ex-info \"no\" {})) \"ok\"))"
                               :tests [{:input {} :expect "ok"}]})))
    (is (not (:ok (call k "boom" {:x 1}))))
    (is (nil? (live k "boom")))))

(deftest soak-keep
  (let [k (open-k :soak 3)]
    (gate/propose! k wc-v2)
    (dotimes [_ 2] (call k "word_count" {:text "a"}))
    (is (= "trial" (:status (live k "word_count"))))
    (call k "word_count" {:text "a"})
    (is (= "kept" (:status (live k "word_count"))))
    (is (re-find #"def word_count v2 .*kept after 3 clean uses" (:text (last (st/verdicts @(:state k))))))
    (testing "runtime errors of a kept version do not revert"
      (let [r (call k "word_count" {:text 42})]
        (is (not (:ok r)))
        (is (= "kept" (:status (live k "word_count"))))))))

(deftest persistence
  (let [k1 (open-k)]
    (gate/propose! k1 wc-v2)
    (call k1 "word_count" {:text "a"})
    (gate/propose! k1 (assoc wc-v2 :source "(defn run [x] (" ))
    (kn/add! k1 "lesson" "Prefer small widget changes" "model")
    (kn/score! k1 ["L1"] true)
    (agent/run-task! (with-script k1 [[(text "done")]]) "touch the widget")
    (let [k2 (open-k)]
      (is (= @(:state k1) @(:state k2)))
      (is (= 1 (get-in @(:state k2) [:defs "word_count" :uses])))
      (is (= 2 (:score (get-in @(:state k2) [:knowledge "L1"]))))
      (testing "soak continues across processes"
        (call k2 "word_count" {:text "a"})
        (call k2 "word_count" {:text "a"})
        (is (= "kept" (:status (live k2 "word_count"))))
        (is (= @(:state k2) @(:state (open-k))))))))

(deftest hook-redefinition
  (let [script [[(tool-use "t1" "propose_def"
                           {:name "compose_system" :kind "hook" :description "custom"
                            :source "(defn compose-system [notes lessons] (str \"CUSTOM SUPPLEMENT \" (count notes)))"
                            :tests [{:input {:notes ["a"] :lessons []} :expect "CUSTOM SUPPLEMENT 1"}]})]
                [(text "done")]]
        k (open-k :script script)
        r (agent/run-task! k "change the prompt")
        [req1 req2] @(:requests (:provider k))]
    (is (:success r))
    (is (not (str/includes? (system-text req1) "CUSTOM SUPPLEMENT")))
    (is (str/includes? (system-text req2) "CUSTOM SUPPLEMENT 0"))
    (is (= (:base-prompt k) (:text (first (:system req2)))))
    (is (str/starts-with? (:base-prompt k) "You are cmh, a coding agent"))))

(deftest lesson-scoring
  (let [k (open-k)
        ok [[(text "done")]]
        fail [[(tool-use "t1" "read_file" {:path "/nonexistent/file"})] [(text "gave up")]]
        run (fn [script] (agent/run-task! (with-script k script) "fix the widget"))
        score #(get-in @(:state k) [:knowledge "L1" :score])]
    (kn/add! k "lesson" "check the widget config first" "model")
    (is (:success (run ok)))
    (is (= 1 (score)))
    (is (not (:success (run fail))))
    (is (= 0 (score)))
    (dotimes [_ 3] (run fail))
    (is (= -3 (score)))
    (is (true? (get-in @(:state k) [:knowledge "L1" :retired])))
    (let [k' (with-script k ok)]
      (agent/run-task! k' "fix the widget")
      (is (not (str/includes? (system-text (first @(:requests (:provider k'))))
                              "check the widget config first")))
      (is (= -3 (score)) "retired lessons are no longer scored"))))

(deftest end-to-end-run
  (let [script [[(text "I'll make a tool.")
                 (tool-use "t1" "propose_def"
                           {:name "reverse" :kind "tool" :description "Reverse a string"
                            :source "(defn run [input] (apply str (reverse (:text input))))"
                            :tests [{:input {:text "abc"} :expect "cba"}]
                            :input_schema {:type "object" :properties {:text {:type "string"}}
                                           :required ["text"]}})]
                [(tool-use "t2" "reverse" {:text "hello"})]
                [(text "olleh")]
                [(text "[\"Write a small tool for repeated string transforms.\"]")]]
        f (io/file *dir* "script.json")
        _ (spit f (json/write-str script))
        err (java.io.StringWriter.)
        out (java.io.StringWriter.)
        {:keys [exit kernel]} (binding [*out* out *err* err]
                                (main/run-cli ["--state" (str *dir* "/state") "--provider" "mock"
                                               "--script" (str f) "run" "reverse hello"]))
        reqs @(:requests (:provider kernel))
        tool-result (-> (nth reqs 2) :messages last :content first)]
    (is (= 0 exit))
    (is (= "olleh\n" (str out)) "stdout holds only the final answer")
    (is (str/includes? (str err) "[tool] propose_def reverse -> trial v1"))
    (is (some #{"reverse"} (map :name (:tools (nth reqs 1)))) "tool list rebuilt per request")
    (is (= "olleh" (:content tool-result)))
    (is (nil? (:tools (nth reqs 3))) "reflection request has no tools")
    (is (some #(= "Write a small tool for repeated string transforms." (:text %))
              (st/lessons @(:state kernel))))
    (testing "a script that runs out is a failure with nonzero exit"
      (spit f "[]")
      (let [r (binding [*out* (java.io.StringWriter.) *err* (java.io.StringWriter.)]
                (main/run-cli ["--state" (str *dir* "/state") "--provider" "mock" "--script" (str f)
                               "--no-reflect" "run" "x"]))]
        (is (= 1 (:exit r)))))))
